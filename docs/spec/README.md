# Specification

What the system is and how it behaves, as shipped. Present tense, stated once.

| File | Contents |
| --- | --- |
| [`00-overview.md`](00-overview.md) | Plakous and Plak, the two-layer rule, supported packages and platforms, the crate map, facade features, persisted schema versions, and how to read these docs |
| [`01-plakous-library.md`](01-plakous-library.md) | The normative behavior of the Plakous library: behavioral model and invariants; construction and lifetime; the agent loop and turn lifecycle; canonical history, checkpoints, and the runtime manifest; sessions, the SQLite store, recovery, fork, and rebind; providers, protocol families, and admission; authentication and revalidation; tools and effects; Code Mode and local programmatic composition; compaction; observation versus authority and telemetry; the public facade; errors, limits, secrets, and performance; evidence; and what Plakous is not |
| [`02-plak-product.md`](02-plak-product.md) | The product: the Plak home and its versioned state directory, the CLI grammar and exit codes, the JSONL host protocol, the TUI — composer, queue, slash commands, keybinds, Menu — prompts and the coding packs, usage, doctor, and the login surfaces |
| [`03-orchestration.md`](03-orchestration.md) | The agent tree: templates, roster and epoch, grants and tiers, spawn, mail, wait, sealing and candidates, adoption, capacity, Code Mode cells, the copy shim, `on_mint`, persistence and reopen, the topology JSONL commands, and the known limits |
| [`04-security-and-boundaries.md`](04-security-and-boundaries.md) | Trust boundaries and the threat register, what is enforced versus convention, credentials and refresh, effects and authority, redaction, panics, unsafe code and supply chain, vulnerability reporting, and residual risk |
| [`05-testing-and-evidence.md`](05-testing-and-evidence.md) | The gate layers, offline test conventions, the evidence classes, live scenarios and the pre-release bar, the reference product homes, known flaky classes, and review focus |
| [`06-build-and-support-surface.md`](06-build-and-support-surface.md) | Support classes and inventories, the crate map and its rules, features, unsafe policy, building and installing, dependency policy, and the maintainer surface |
| [`07-config-reference.md`](07-config-reference.md) | Every key `plak` reads from disk: files and discovery, the grammar, `config.toml`, `agents/<name>.toml`, the named load and save errors, the epoch digest rule, the `plak config set` grammar, and the shipped presets |

Operator-facing material lives in [`../user-guide/`](../user-guide/).

Decision history is not tracked in this repository. This specification, not any
archive, states current behavior.

## Glossary

- **Plakous L0** — the provider-neutral single-agent runtime and facade; see
  [the crate-set rule](00-overview.md#the-crate-set-rule).
- **Plak** — the CLI, TUI and JSONL product built on Plakous; see
  [the product specification](02-plak-product.md).
- **route tuple** — the exact account, provider, model, effort, composition and
  protocol selected for a run; evidence is scoped to it as described in
  [Testing and evidence](05-testing-and-evidence.md).
- **canonical history** — durable model-facing conversation authority, distinct
  from displayed text; see [Plakous](01-plakous-library.md).
- **agent tree** — Plak's composition of isolated Plakous agents, including
  grants, workspaces and result delivery; see [Orchestration](03-orchestration.md).
- **admission** — the final pre-dispatch capacity and authority decision; see
  [Plakous](01-plakous-library.md) and [Security](04-security-and-boundaries.md).
- **live qualification** — paid, opt-in evidence for one exact tested tuple and
  revision. The pre-release bar is
  [§3.4](05-testing-and-evidence.md#34-the-pre-release-bar); scenario vocabulary
  is [live scenarios](05-testing-and-evidence.md#32-live-scenarios).

## Conventions

- **MUST**, **MUST NOT**, **SHOULD**, and **MAY** mark boundaries where
  omission would change safety, portability, or a claimed contract. Everything
  else is ordinary description.
- Rules are stated once, in the place that owns them, with no amendment layers
  and no wave or phase framing.
- Code is the authority for exact inventories, constants, and schemas; where a
  number appears here it names the file that holds it.
- A claim about a provider route, model, or capacity is scoped to the exact
  evidence that admits it. Nothing here promotes an integration, evidence
  class, provider tuple, or recovery revision.
