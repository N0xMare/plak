# Plakous and Plak: system overview

Plakous is an embeddable, multi-provider, **single-agent** harness library
written in Rust. It runs one capable agent through one provider-neutral control
flow while preserving each protocol family's real wire semantics. It owns the
agent loop, canonical conversation, tools and effects, harness-plan
compilation, provider integrations, authentication, admission and
revalidation, durability, and semantic session recovery.

Plak is the product built on it: a terminal application, CLI, and long-lived
JSONL host that composes Plakous agents into a workspace-shaped coding tool
with configuration, queues, presentation, and multi-agent topology.

## The crate-set rule

**L0 is exactly four crates** — `plakous-model`, `plakous-tools`,
`plakous-agent`, and the `plakous` facade — **and L0 has no topology.** One
`Agent` owns exactly one canonical conversation and at most one active root
turn; no Plakous L0 file counts topology names. Consumers may build many
Plakous agents and compose them, but each agent stays an isolated single-agent
runtime, and multi-agent composition does not enter L0 without a change to this
chapter.

Every other `crates/**` package is a Plakous library in one of two positions.
**Below the facade** — transports, kernels, providers, the store, the engines,
the harness packs and prompts — composed behind the facade. Plak and the
host-layer libraries reach these through the facade; direct package nameability
does not confer a support promise. **Above the facade** — `plakous-host-protocol`,
`plakous-workspace`, `plakous-recursive-subagents`,
`plakous-recursive-subagents-store`, and `plakous-hosted` — which consume only
`plakous` and each other and are never re-exported by it. The facade is the only
L0 complete-workflow surface.

The product is one binary crate, `bin/plak`. It composes the crates above the
facade and holds every product opinion.

| Position | Owns | Does not own |
| --- | --- | --- |
| Plakous L0 (`plakous-model`, `plakous-tools`, `plakous-agent`, `plakous`) | the loop, canonical history, checkpoints, tools and effects, profiles and resolved plans, providers and dialects, credentials, admission, revalidation, durable store and its change feed, recovery | UI, product configuration and defaults, queues, workflow, live-test procedure, topology |
| Plakous above the facade (`plakous-host-protocol`, `plakous-workspace`, `plakous-recursive-subagents`, `plakous-recursive-subagents-store`, `plakous-hosted`) | the neutral host wire vocabulary and its typed views, agent workspaces over `git`/`jj`, the durable agent tree and its store, and one hosted session — build, open, resume, the host loop, the queue and the session intents, the one change-feed projector and its views, children, catalog, and binding | product configuration, the CLI grammar, the terminal interface, the homes, and the product and telemetry stores |
| Plak (`bin/plak`) | CLI, TUI, the JSONL transport, product configuration and defaults, the Plak home, the product and telemetry stores, agent templates and the roster, account admission for children, the tree's presentation and its store location, live-test procedure | the agent loop, canonical history, provider dispatch, credentials, checkpoint authority, the hosted session loop |

Plak resolves its defaults, makes them visible, and passes a **complete**
configuration into the facade. Product defaults are never resume or
revalidation authority. Plak never calls a provider directly, never keeps a
second inference loop, and never reconstructs canonical history from displayed
text. An account, model, or route change is a validated construction input that
creates an explicit rebind or fork boundary, not an in-place mutation.

## Packages and support classes

Publication and Rust nameability confer accessibility, not support. A published
package carries no direct-use compatibility, qualification, or support promise
unless it is in the supported class.

The workspace contains 40 packages: five supported, 32 published internal
(the vendored `plakous-codex-code-mode-protocol` among them), and three
repository-only. The supported and published-internal packages are publishable
to crates.io as one exact cohort; the repository-only packages set
`publish = false` (chapter 06 §1).

- **Supported:** `plakous`, `plakous-agent`, `plakous-model`, `plakous-tools`,
  `plak`. Each has an exact inventory pinned by a compile fixture or process
  test, not by whatever happens to be `pub`.
- **Published internal:** the common, engine, protocol, provider, store,
  harness, host, and vendor crates listed in the crate map below.
- **Repository-only:** `plakous-test-support`,
  `plakous-offline-crash-harness`, and `plakous-mcp-fake-server`.

Complete-workflow embedders depend on `plakous` alone. Narrow component
embedders may use `plakous-agent`, `plakous-model` (Custom model integration),
or `plakous-tools` (Custom tool integration).

## Platforms

The workspace is Rust edition 2024 with MSRV 1.99.0 and forbids unsafe Rust in
production code.

| Target | Status |
| --- | --- |
| `x86_64-unknown-linux-musl` | Supported; process tools and evaluator workers run with host authority, without an OS sandbox (chapter 04) |
| `aarch64-apple-darwin` | Supported; process tools and evaluator workers run with host authority, without an OS sandbox (chapter 04) |
| `x86_64-apple-darwin` | Not supported for process tools or Code Mode; fails closed |

SQLite is statically embedded (3.53.2) with the reviewed compile options.

## Crate map

Every workspace member declares its architectural `layer` and support class in
`[package.metadata.plakous]`. The fuzz crate and `vendor/monty-typeshed` are
not workspace members and carry no such table. Libraries are grouped by
responsibility under `crates/`; the complete-workflow crate is
`crates/plakous`, and the product is `bin/plak`. Directory groups do not
authorize dependency edges. Chapter 06 §3 owns the legal dependencies, support
classes and directory policy.

| Crate | Directory | Layer | Role |
| --- | --- | --- | --- |
| `plakous-model` | `crates/core/model` | `core` | Provider-neutral model contracts: the object-safe `LanguageModel` boundary, validated descriptors and capabilities, canonical tool definitions and schemas, deterministic response assembly |
| `plakous-tools` | `crates/core/tools` | `core` | Portable tool contracts, registries, exposure maps, execution environments and policy, the `ToolPack` seam and its compiler, durable composition markers and the standard identity vocabulary |
| `plakous-agent` | `crates/core/agent` | `core` | The single-agent runtime: private single-owner driver, canonical history, checkpoints, tool/effect scheduling, compaction, telemetry sink. Implementation lives in the unsupported `internal` tier of the same package |
| `plakous-attempt` | `crates/common/attempt` | `common` | Family-agnostic attempt lifecycle: phase transitions, observation ledger, pure acceptance outcomes |
| `plakous-http` | `crates/common/http` | `common` | Provider-blind bounded HTTP transport and SSE framing. One fresh connection per execute unless the client is explicitly pooled; no redirect, proxy, retry, or auth semantics |
| `plakous-auth` | `crates/common/auth` | `common` | Provider-neutral auth foundations: secret containers, OAuth pins and workflow, callback binding, credential metadata, durable private storage |
| `plakous-js-eval` | `crates/runtimes/js-eval` | `engine` | Evaluator protocol and sidecar client; never links the QuickJS engine |
| `plakous-js-runtime` | `crates/runtimes/js-runtime` | `engine` | QuickJS runtime for the `plakous-js-host` binary and opt-in library embedding |
| `plakous-py-worker` | `crates/runtimes/py-worker` | `engine` | Thin `monty` subprocess entry point over the pinned upstream interpreter and wire state machine |
| `plakous-py-eval` | `crates/runtimes/py-eval` | `engine` | monty-pool py-ptc session: cell lifecycle, nested-call delivery, JSON/Monty codec, py-ptc refusal strings |
| `plakous-code-mode` | `crates/runtimes/code-mode` | `engine` | Parse/DTO helpers for the Code Mode presentation (source parsing, tool-description rendering, result truncation). Not an evaluator |
| `plakous-responses` | `crates/protocols/responses` | `protocol` | Responses-family kernel: wire grammar, stream framing, limits, sequencing |
| `plakous-responses-dialect` | `crates/protocols/responses-dialect` | `protocol` | Generic Responses dialect host shapes shared by the Responses commercial routes |
| `plakous-messages` | `crates/protocols/messages` | `protocol` | Messages-family kernel: request grammar, block ordering, thinking/signature replay, stream grammar, wire accounting |
| `plakous-chat-completions` | `crates/protocols/chat-completions` | `protocol` | Chat Completions kernel: message lowering, one-choice streaming lifecycle, indexed function calls, exact usage, SSE `[DONE]` |
| `plakous-exchange` | `crates/protocols/exchange` | `protocol` | Physical-exchange plumbing shared by the kernels and providers: transport failure to network-failure facts, bounded error-body reads, the cancellation race, and the turn-retry decline ladder |
| `plakous-openai` | `crates/providers/openai` | `provider` | OpenAI route/catalog/authz dialect plus Codex OAuth subscription execution |
| `plakous-anthropic` | `crates/providers/anthropic` | `provider` | Anthropic Messages dialect: route, authz, catalog, error hooks, Claude OAuth costume |
| `plakous-xai` | `crates/providers/xai` | `provider` | xAI Responses provider facts and SuperGrok subscription auth |
| `plakous-openrouter` | `crates/providers/openrouter` | `provider` | Dynamic OpenRouter route, discovery, authz, and capability tables across three wire dialects |
| `plakous-store` | `crates/store` | `store` | `SessionStore` backends: bounded in-memory by default, SQLite behind the `sqlite` feature |
| `plakous-prompts` | `crates/harness/prompts` | `harness` | Model-visible instruction text, the first-turn assembler, the recommended profiles, and `AGENTS.md` discovery. Depends on no pack |
| `plakous-compose` | `crates/harness/compose` | `harness` | The js/py cell outer tools, the Codex Code Mode `exec`/`wait` compilers, nested catalogs, and the `SidecarLocator` port |
| `plakous-tool-files` | `crates/harness/tool-files` | `harness` | Bounded reads, exact-content patching, image inspection, and the Codex patch envelope, behind `ToolPack` |
| `plakous-tool-search` | `crates/harness/tool-search` | `harness` | Bounded deterministic repository search and directory listing, behind `ToolPack` |
| `plakous-tool-web` | `crates/harness/tool-web` | `harness` | Local wrapper tools for provider-hosted web search, page fetch and X search over a `RetrievalPort`, behind `ToolPack` |
| `plakous-tool-process` | `crates/harness/tool-process` | `harness` | Joined process sessions and the per-vocabulary shell tool, behind `ToolPack` |
| `plakous-mcp` | `crates/harness/mcp` | `harness` | The MCP client: the process-wide stdio and streamable HTTP server pool, frozen tool listings, input-schema lowering, `mcp__<server>__<tool>` naming, the serial-mutating MCP tool pack and its `read_resource` tool, resource and prompt operations, and MCP OAuth over `plakous-auth` |
| `plakous-tool-plan` | `crates/harness/tool-plan` | `harness` | The `PlanState` port, `update_plan`, and the id-merge `todo_write` lowering, behind `ToolPack` |
| `plakous` | `crates/plakous` | `facade` | Public facade: curated re-exports, session workflows, feature closures. The only dependency a complete-workflow embedder needs, and the only L0 complete-workflow surface |
| `plakous-host-protocol` | `crates/hosting/host-protocol` | `host` | Provider-neutral host wire types shared by the hosted session and the product: session intents and observations, their bounded channels, the typed views, the topology wire vocabulary, the presentation limits, and the JSONL projection |
| `plakous-workspace` | `crates/orchestration/workspace` | `host` | Agent workspaces over `git`/`jj`: detect, mint, seal, adopt, unmint, and the git-redirect fence |
| `plakous-recursive-subagents` | `crates/orchestration/recursive-subagents` | `host` | One durable agent tree per root session, its epoch, its tools and their model-visible text, and its hub |
| `plakous-recursive-subagents-store` | `crates/orchestration/recursive-subagents-store` | `host` | That tree's durable store, over its own `subagents.sqlite3` |
| `plakous-hosted` | `crates/hosting/hosted` | `host` | One hosted session: build, open and resume, the host loop and the detached shell loop, the one-shot drive, the queue and session intents, the change-feed projector, child binding, the ready catalog, and the session record and its ports |
| `plak` | `bin/plak` | `bin` | The product: CLI grammar and the `plak` binary, the Ratatui presentation shell as the `tui` module, the JSONL stdio transport, the Plak home, product configuration, the product and telemetry stores, and the composition root |

Two artifacts inside those packages are worth naming because they are often
mistaken for packages of their own. `bin/plak/src/tui/` is an inert
presentation module, not a crate: it imports only `plakous-host-protocol`
values and the binary's own product settings vocabulary, a constraint the
workspace policy checks per module. `plakous-js-host` is not a package either:
it is a `[[bin]]` of `plakous-js-runtime`, and it is the rquickjs worker for Code Mode and js-ptc.

The Monty interpreter and protocol remain external dependencies pinned at
`=1.2.0`; only their `monty-typeshed` dependency resolves to a reviewed
vendor snapshot (chapter 06 §7.3).
`plakous-py-worker` supplies the process entry point;
`plakous-py-eval` owns only the session that drives that worker.

`plakous-code-mode` consumes the reviewed vendor snapshot at
`vendor/codex-code-mode-protocol`, packaged as
`plakous-codex-code-mode-protocol` (upstream `codex-code-mode-protocol`, renamed),
which carries only the description and schema-rendering closure Plakous calls,
with its upstream identity and license recorded in its `PROVENANCE.md`. It is
the one member of the `vendor` layer, with no internal dependencies, and keeps
its upstream edition, rust-version, license and lints; `plakous-code-mode`
depends on it by path and exact version, so no consumer carries a Codex
`[patch]` table.

## Facade features

`plakous` default features are empty. The embedder-safe workflow surface —
agent, application, construction, sessions, model, tools, profiles, `VERSION` — is always on.

| Feature | Gates |
| --- | --- |
| `openai`, `xai`, `anthropic`, `openrouter` | Re-export that provider crate as `plakous::<name>`. Provider internals, not a presentation surface |
| `auth` | `plakous::auth` secret, credential-store, OAuth, pin, and account-binding foundations; implies `openai` and `xai` |
| `composition` | The local composition attachment seam in `plakous::agent` (`LocalCompositionAttachment`, `attach_local_composition`): Code Mode presentation and JavaScript evaluation through the `plakous-js-host` sidecar. It reaches no credential, HTTP, subscription, or Python machinery |
| `composition-python` | The Monty-backed Python PTC projection over the same composition API; implies `composition` |
| `native-subscriptions` | The subscription tier: `subscriptions` and the native-auth facets of the provider crates. It implies `composition` and `composition-python`, which is Plak's product closure |
| `sessions-sqlite` | The SQLite `SessionStore` backend |
| `full` | All of the supported features above |

Evaluator capability and subscription authentication are separate choices. A
Custom-model embedder that wants JavaScript selects `composition` and links
neither `plakous-auth` nor `plakous-http` nor the Python evaluator. No facade
consumer needs a `[patch]` table; chapter 06 states the one that building the
Monty worker package needs.

## Persisted versions

| Store | Owner | Current version |
| --- | --- | --- |
| Runtime session store (`runtime.sqlite3`), including the change feed | Plakous | SQLite schema **25** |
| Runtime checkpoint graph and feed records (inside `runtime.sqlite3`) | Plakous | format epoch **28** (reconstructive process discovery ceiling) |
| Product store (`product.sqlite3`): intents and feed-derived views | Plak | schema **33** |
| Agent tree store (`subagents.sqlite3`): intents, feed-derived views and external-effect records | `plakous-recursive-subagents-store` (schema), Plak (location) | schema **8** |
| Telemetry store (`telemetry/telemetry-v<schema>.sqlite3`) | Plak | schema **19**, disposable, never recovery authority |
| Session export document | Plak | version **8**, imports **8** and **7** |

Tree schema 8's unreleased canonical `seal_outcome` payload includes the durable
`history_left_base` boolean used by replayed result-mail notices. Runtime epoch
28 moves the whole state key, so no older t8 shape is opened. Export v8 carries
conversation history and CarryBinding, not tree seal outcomes; it is unchanged.

There are no released persisted-format obligations. Each store has exactly one
supported version, and nothing is migrated. The session export document is the
cross-build carrier exception: a build imports documents of its own version and
of the one before, and no other store crosses between builds. A pre-release
format change bumps the store's schema or the runtime format epoch; the
per-component revision constants inside the checkpoint graph are frozen. Plak
keeps each build's stores in a state directory named by all four versions
(chapter 02 §2.5), currently `r25.e28.p33.t8`, so a build never opens files from another version tuple as
session authority. Explicit legacy pruning may take schema-blind SQLite and
holder locks to prove a directory is
unused (chapter 02 §2.5). A session another build wrote is
refused with one message (chapter 01 §5.4).

## How to read the rest of these docs

Two directories are tracked, and together they are the documentation:

- **`docs/spec/`** — what the system is and how it behaves.
  - `00-overview.md` (this file) — the crate-set rule, packages, platforms,
    the crate map, and version facts.
  - `01-plakous-library.md` — the normative behavior of the library: turn
    lifecycle, kernel and checkpoint authority, store and recovery, providers
    and admission, auth, tools, Code Mode, compaction, telemetry, facade,
    errors.
  - `02-plak-product.md` — the product: the Plak home and its stores, the CLI
    grammar and exit codes, the JSONL host protocol, the TUI, prompts and the
    coding packs, usage, doctor, and the login surfaces.
  - `03-orchestration.md` — the agent tree: templates and roster, grants and
    tiers, spawn, mail, sealing, adoption, capacity, cells, the copy shim,
    `on_mint`, persistence and reopen, and the known limits.
  - `04-security-and-boundaries.md` — the threat register, enforced versus
    convention, credentials, effects and authority, redaction, panics, unsafe
    code and supply chain, and residual risk.
  - `05-testing-and-evidence.md` — the gate layers, offline test conventions,
    the evidence classes, live scenarios, the pre-release bar, known flaky
    classes, and review focus.
  - `06-build-and-support-surface.md` — support classes and inventories, the
    crate map's rules, features, building and installing, dependency policy,
    and the maintainer surface.
  - `07-config-reference.md` — every key `plak` reads from disk, with its type,
    default, constraint, and meaning, plus the shipped presets.
- **`docs/user-guide/`** — operator-facing material: installing and running
  Plak, logging in, configuring agents and templates, the TUI, and the CLI and
  JSONL surfaces.

Decision history is not tracked in this repository. This specification, not any
archive, states current behavior.

Normative statements are present tense. **MUST**, **MUST NOT**, **SHOULD**, and
**MAY** appear only where omission would change safety, portability, or a
claimed contract.
