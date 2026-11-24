# Plakous and plakd: system overview

Plakous is an embeddable, multi-provider, **single-agent** harness library
written in Rust. It runs one capable agent through one provider-neutral control
flow while preserving each protocol family's real wire semantics. It owns the
agent loop, canonical conversation, tools and effects, harness-plan
compilation, provider integrations, authentication, admission and
revalidation, durability, and semantic session recovery.

plakd is the product built on it: a host server, operator/batch CLI, and long-lived
JSONL transport that composes Plakous agents into a workspace-shaped coding tool
with configuration, queues, presentation, and multi-agent topology.

## The crate-set rule

**L0 is exactly four crates** — `plakous-model`, `plakous-tools`,
`plakous-agent`, and the `plakous` facade — **and L0 has no topology.** One
`Agent` drives one branch of exactly one **agent line** and at most one active
root turn. A line is durable single-agent lineage: its current head branch is
the only branch that commits; a head-advancing rebind or a same-store carry
moves the head onto a new branch of the same line, and a fork starts a new
line. A line has no nodes, parents or children, and no Plakous L0 file counts
topology names. Consumers may build many
Plakous agents and compose them, but each agent stays an isolated single-agent
runtime, and multi-agent composition does not enter L0 without a change to this
chapter.

Except for the product-owned `plakd-wire-types`, every other `crates/**`
package is a Plakous library in one of two positions.
**Below the facade** — transports, kernels, providers, the store, the engines,
the harness packs and prompts -- composed behind the facade. plakd and the
host-layer libraries reach these through the facade, apart from the one
recorded exception below; direct package nameability
does not confer a support promise. **Above the facade** — `plakous-host-protocol`,
`plakous-workspace`, `plakous-recursive-subagents`,
`plakous-recursive-subagents-store`, and `plakous-hosted` — which consume only
`plakous` and each other, except for `plakous-hosted`'s recorded edge to the
common `plakous-image` (chapter 06 §3), and are never re-exported by it. The facade is the only
L0 complete-workflow surface.

`plakd-wire-types` occupies the existing `host` layer and depends on
`plakous-host-protocol` for serialized product extension vocabulary. It owns
no product handlers, configuration, persistence or runtime authority.

The product is one binary crate, `bin/plakd`. It composes the crates above the
facade and owns product policy and execution; its serialized extension
vocabulary is shared through `plakd-wire-types`.

Product catalog construction and attachment eligibility are host services,
independent of client presentation. CLI inspection and hosted session options
consume the same product-owned catalog facts.

| Position | Owns | Does not own |
| --- | --- | --- |
| Plakous L0 (`plakous-model`, `plakous-tools`, `plakous-agent`, `plakous`) | the loop, canonical history, checkpoints, tools and effects, profiles and resolved plans, providers and dialects, credentials, admission, revalidation, durable store and its change feed, recovery | UI, product configuration and defaults, queues, workflow, live-test procedure, topology |
| Plakous above the facade (`plakous-host-protocol`, `plakous-workspace`, `plakous-recursive-subagents`, `plakous-recursive-subagents-store`, `plakous-hosted`) | the neutral host wire vocabulary and its typed views, agent workspaces over `git`/`jj`, the durable agent tree and its store, and one hosted session -- build, open, resume, the host loop, the queue and the session intents, the one change-feed projector and its views, children, catalog, and binding | product configuration, the CLI grammar, application clients, the homes, and the product and telemetry stores |
| plakd (`bin/plakd`) | operator CLI, host and JSONL transport, product configuration and defaults, the plakd home, the product and telemetry stores, agent templates and the roster, account admission for children, the tree's presentation and its store location, live-test procedure | the agent loop, canonical history, provider dispatch, credentials, checkpoint authority, the hosted session loop |

plakd resolves its defaults, makes them visible, and passes a **complete**
configuration into the facade. Product defaults are never resume or
revalidation authority. plakd never calls a provider directly, never keeps a
second inference loop, and never reconstructs canonical history from displayed
text. An account, model, or route change is a validated construction input that
creates an explicit rebind or fork boundary, not an in-place mutation.

## Packages and support classes

Publication and Rust nameability confer accessibility, not support. A published
package carries no direct-use compatibility, qualification, or support promise
unless it is in the supported class.

The workspace contains 39 packages: five supported, 31 published internal
(the vendored `plakous-codex-code-mode-protocol` among them), and three
repository-only. The supported and published-internal packages are publishable
to crates.io as one exact cohort; the repository-only packages set
`publish = false` (chapter 06 §1).

- **Supported:** `plakous`, `plakous-agent`, `plakous-model`, `plakous-tools`,
  `plakd`. Each has an exact inventory pinned by a compile fixture or process
  test, not by whatever happens to be `pub`.
- **Published internal:** the common, engine, protocol, provider, store,
  harness, host, and vendor crates listed in the crate map below.
- **Repository-only:** `plakous-test-support`,
  `plakous-offline-crash-harness`, `plakous-mcp-fake-server`.

Complete-workflow embedders depend on `plakous` alone. Narrow component
embedders may use `plakous-agent`, `plakous-model` (Custom model integration),
or `plakous-tools` (Custom tool integration).

## Platforms

The workspace is Rust edition 2024 with MSRV 1.100.0 and forbids unsafe Rust in
production code.

| Target | Status |
| --- | --- |
| `x86_64-unknown-linux-musl` | Supported; process tools and evaluator workers run with host authority, without an OS sandbox (chapter 04) |
| `aarch64-apple-darwin` | Supported; process tools and evaluator workers run with host authority, without an OS sandbox (chapter 04) |
| `x86_64-apple-darwin` | Not supported for process tools or Code Mode; fails closed |

SQLite is statically embedded (3.54.0) with the reviewed compile options.

## Crate map

Every workspace member declares its architectural `layer` and support class in
`[package.metadata.plakous]`. The fuzz crate is not a workspace member and carries no such table. Libraries are grouped by
responsibility under `crates/`; the complete-workflow crate is
`crates/plakous`, and the product is `bin/plakd`. Directory groups do not
authorize dependency edges. Chapter 06 §3 owns the legal dependencies, support
classes and directory policy.

| Crate | Directory | Layer | Role |
| --- | --- | --- | --- |
| `plakous-model` | `crates/core/model` | `core` | Provider-neutral model contracts: the object-safe `LanguageModel` boundary, validated descriptors and capabilities, canonical tool definitions and schemas, deterministic response assembly |
| `plakous-tools` | `crates/core/tools` | `core` | Portable tool contracts, registries, exposure maps, execution environments and policy, the `ToolPack` seam and its compiler, durable composition markers and the standard identity vocabulary |
| `plakous-agent` | `crates/core/agent` | `core` | The single-agent runtime: private single-owner driver, canonical history, checkpoints, tool/effect scheduling, compaction, telemetry sink. Implementation lives in the unsupported `internal` tier of the same package |
| `plakous-http` | `crates/common/http` | `common` | Provider-blind bounded HTTP transport and SSE framing. One fresh connection per execute unless the client is explicitly pooled; no redirect, proxy, retry, or auth semantics |
| `plakous-auth` | `crates/common/auth` | `common` | Provider-neutral auth foundations: secret containers, OAuth pins and workflow, callback binding, credential metadata, durable private storage |
| `plakous-runner-protocol` | `crates/common/runner-protocol` | `common` | Runner protocol (chapter 01 §8.4): length-prefixed JSON frames, the hello, bind and paired read-only qualification handshake, request classes, owner transfer, fixed Changes reads, separate operator-terminal control and answers |
| `plakous-gateway-protocol` | `crates/common/gateway-protocol` | `common` | Portable credential gateway protocol (chapter 01 §7.5): relay envelope, response frames and their bounded decoder, refusals, account facts, admin calls, caller keys and scopes; no product caller or I/O |
| `plakous-image` | `crates/common/image` | `common` | The one image sniffer (re-exported by `plakous-tools`), the attachment count and per-message byte budget, and, under `normalize`, the one attachment and tool-result normalizer and image-file reader: decode, bound, downscale and re-encode, clipboard RGBA, the 64 MiB unread refusal; no runtime, and it reads only a caller-named file |
| `plakous-cell` | `crates/runtimes/cell` | `engine` | Cell protocol (`plakous-cell-protocol.v2`), worker client and session for isolated interpreter workers; never links an interpreter |
| `plakous-js-runtime` | `crates/runtimes/js-runtime` | `engine` | QuickJS runtime for the `plakous-js-host` binary and opt-in library embedding |
| `plakous-py-worker` | `crates/runtimes/py-worker` | `engine` | The `plakous-py-host` cell worker: the pinned micro-monty interpreter on the cell protocol, the host-value codec, nested-call batching and acceptance, worker-side refusal strings |
| `plakous-runner` | `crates/runtimes/runner` | `engine` | The runner protocol's host end (a runner link) and runner end (owner and op tables over an execution environment), joined in process as a loopback for conformance; reached through the facade's hidden `plakous::runner` |
| `plakous-responses` | `crates/protocols/responses` | `protocol` | Responses-family wire grammar, stream lifecycle, projection and shared dialect hosts |
| `plakous-messages` | `crates/protocols/messages` | `protocol` | Messages-family kernel: request grammar, block ordering, thinking/signature replay, stream grammar, wire accounting |
| `plakous-chat-completions` | `crates/protocols/chat-completions` | `protocol` | Chat Completions kernel: message lowering, one-choice streaming lifecycle, indexed function calls, exact usage, SSE `[DONE]` |
| `plakous-exchange` | `crates/protocols/exchange` | `protocol` | Shared physical-generation host, attempt lifecycle, observation ledger and retry policy for all three families; the canonical Chat/Messages attempt sink, bounded catalog transport, and isolated side requests |
| `plakous-openai` | `crates/providers/openai` | `provider` | OpenAI route/catalog/authz dialect plus Codex OAuth subscription execution |
| `plakous-anthropic` | `crates/providers/anthropic` | `provider` | Anthropic Messages dialect: route, authz, catalog, error hooks, Claude OAuth costume |
| `plakous-xai` | `crates/providers/xai` | `provider` | xAI Responses provider facts and SuperGrok subscription auth |
| `plakous-openrouter` | `crates/providers/openrouter` | `provider` | Dynamic OpenRouter route, discovery, authz, and capability tables across three wire dialects |
| `plakous-store` | `crates/store` | `store` | `SessionStore` backends: bounded in-memory by default, SQLite behind the `sqlite` feature |
| `plakous-prompts` | `crates/harness/prompts` | `harness` | Model-visible instruction text, the first-turn assembler, the recommended profiles, and `AGENTS.md` and Agent Skills discovery. Depends on no pack |
| `plakous-compose` | `crates/harness/compose` | `harness` | Cell presentations and their single compiler, the host-API descriptor and TypeScript/Python generators, Codex exec parsing/DTO helpers, nested catalogs, and the `SidecarLocator` port |
| `plakous-tool-files` | `crates/harness/tool-files` | `harness` | Bounded reads, exact-content patching and image inspection; separate repository search and revisioned `PlanState` packs |
| `plakous-tool-web` | `crates/harness/tool-web` | `harness` | Local wrapper tools for provider-hosted web search, page fetch and X search over a `RetrievalPort`, behind `ToolPack` |
| `plakous-tool-process` | `crates/harness/tool-process` | `harness` | Joined process sessions and the per-vocabulary shell tool, behind `ToolPack` |
| `plakous-mcp` | `crates/harness/mcp` | `harness` | The MCP client: the process-wide stdio and streamable HTTP server pool, frozen tool listings, input-schema lowering, `mcp__<server>__<tool>` naming, the serial-mutating MCP tool pack and its `read_resource` tool, resource and prompt operations, and MCP OAuth over `plakous-auth` |
| `plakous` | `crates/plakous` | `facade` | Public facade: curated re-exports, session workflows, feature closures. The only dependency a complete-workflow embedder needs, and the only L0 complete-workflow surface |
| `plakous-host-protocol` | `crates/hosting/host-protocol` | `host` | Provider-neutral host wire types shared by the hosted session and the product: session intents and observations, their bounded channels, the typed views, the topology wire vocabulary, the presentation limits, and the JSONL projection |
| `plakd-wire-types` | `crates/hosting/plakd-wire-types` | `host` | Serialized product DTOs and extension classification over the host protocol; no settings, store, probe or mailbox authority |
| `plakous-workspace` | `crates/orchestration/workspace` | `host` | Agent workspaces over `git`/`jj`: detect, mint, seal, adopt, unmint, and the git-redirect fence |
| `plakous-recursive-subagents` | `crates/orchestration/recursive-subagents` | `host` | One durable agent tree per root session, its epoch, its tools and their model-visible text, and its hub |
| `plakous-recursive-subagents-store` | `crates/orchestration/recursive-subagents-store` | `host` | That tree's durable store, over its own `subagents.sqlite3` |
| `plakous-hosted` | `crates/hosting/hosted` | `host` | One hosted session: build, open and resume, the host loop and the detached shell loop, the one-shot drive, the queue and session intents, the change-feed projector, child binding, the ready catalog, and the session record and its ports |
| `plakd` | `bin/plakd` | `bin` | The product: CLI grammar and the `plakd` binary, the JSONL stdio transport, the plakd home, product configuration, the product and telemetry stores, and the composition root |

`plakous-js-host` is not a package:
it is a `[[bin]]` of `plakous-js-runtime`, and it is the rquickjs worker for Code Mode and js-ptc.

`plakous-py-worker` builds the `plakous-py-host` worker, embedding the exact
micro-monty release. The host side is the same engine-free `plakous-cell`
client and session as for JavaScript.

`plakous-compose::cell::codex_exec` consumes the reviewed vendor snapshot at
`vendor/codex-code-mode-protocol`, packaged as
`plakous-codex-code-mode-protocol` (upstream `codex-code-mode-protocol`, renamed),
which carries only the description and schema-rendering closure Plakous calls,
with its upstream identity and license recorded in its `PROVENANCE.md`. It is
the one member of the `vendor` layer, with no internal dependencies, and keeps
its upstream edition, rust-version, license and lints; `plakous-compose`
depends on it by path and exact version, so no consumer carries a Codex
`[patch]` table.

## Facade features

`plakous` default features are empty. The embedder-safe workflow surface —
agent, application, construction, sessions, model, tools, profiles, `VERSION` — is always on.

| Feature | Gates |
| --- | --- |
| `openai`, `xai`, `anthropic`, `openrouter` | Re-export that provider crate as `plakous::<name>`. Provider internals, not a presentation surface |
| `auth` | `plakous::auth` secret, credential-store, OAuth, pin, and account-binding foundations; enables no provider |
| `web-fetch` | The independent plakd-run `fetch_url` HTTP tool and HTML-to-Markdown converter; enables no provider |
| `composition` | The local composition attachment seam in `plakous::agent` (`LocalCompositionAttachment`, `attach_local_composition`): Code Mode and JavaScript through the engine-free cell client and explicit worker artifacts. It reaches no credential, HTTP, subscription, or interpreter machinery |
| `composition-python` | Explicitly admits Python PTC and Python SDK profiles through the same cell client; implies `composition`, never an interpreter |
| `codex`, `grok`, `claude` | Each selects exactly its native subscription implementation, without a provider re-export or evaluator. Subscription vocabulary remains readable without any route |
| `mcp` | The MCP client (servers, tools, resources, prompts and MCP OAuth) through `plakous::mcp`. The frozen `McpToolRecord` vocabulary remains available without the client or its transports. The client links `plakous-auth` for MCP OAuth without exposing `plakous::auth`; of the other features only `full` enables it |
| `gateway` | The credential gateway (chapter 01 §7.5) through the doc-hidden, unsupported `plakous::gateway` module: the wire protocol, and as they land the remote custodian and the relay server core. Implies `auth`; enables no provider and no JWT crypto. plakd always enables it |
| `store` | `plakous::sessions`: the session workflows, the feed, portable histories and the process-memory `SessionStore` |
| `sessions-sqlite` | The SQLite `SessionStore` backend; implies `store` |
| `full` | All of the supported features above |

Evaluator capability and subscription authentication are separate choices. A
Custom-model embedder that wants JavaScript or Python selects `composition` and links
neither `plakous-auth` nor `plakous-http` nor an interpreter. No facade or worker consumer needs a `[patch]` table.

## Persisted versions

Backward compatibility is not preserved outside the rules below.
Remove obsolete paths rather than add compatibility layers, fallbacks or
migrations. Unshipped APIs and databases have no compatibility claim.

The latest workspace release is **0.0.4**. This build opens only its current
state key, `r28.e39.p39.t11`, as native authority (chapter 02 §2.5).
Explicit legacy pruning may take schema-blind SQLite and holder locks
to prove another state directory is unused.
The 0.0.4 release uses `r28.e38.p38.t11`; migrate its settled sessions
offline using export and import (chapter 02 §2.5.1).

| Store | Owner | Current version |
| --- | --- | --- |
| Runtime session store (`runtime.sqlite3`), including the change feed | Plakous | SQLite schema **28** |
| Runtime checkpoint graph and feed records (inside `runtime.sqlite3`) | Plakous | format epoch **39**, context codec **4** |
| Product store (`product.sqlite3`): intents and feed-derived views | plakd | generation **39**, revision **41** |
| Agent tree store (`subagents.sqlite3`): intents, feed-derived views and external-effect records | `plakous-recursive-subagents-store` (schema), plakd (location) | generation **11**, revision **13** |
| Telemetry store (`telemetry/telemetry-v<schema>.sqlite3`) | plakd | schema **22**, disposable, never recovery authority |
| Session export document | plakd | writes **14**, imports **14** and **13**, refuses **12** and older |

Bump the runtime format epoch only when persisted runtime encoding changes,
not on a release or protocol cadence. Checkpoint component revisions are
frozen. Epoch 39 is the last declared format cut. The runtime reads the
current and immediately previous record epochs in place. No store under
the epoch-39 key was written with epoch-38 records; stores under the
epoch-38 key still require offline migration. Non-cut bumps leave the key's
epoch component at the last declared cut, separate from the writer epoch
(chapter 02 §2.5). Batch incompatible stored-format changes into declared
cuts. Running host upgrades stay within their current key. Migrate across
keys offline with settled export and import, never by opening a foreign key
as native session authority.

The first exception is the session export document. A build MUST import
export documents of its own version and of the immediately previous version;
the previous importer is kept for one bump only. Older builds need not import
newer documents. Changes to the document, including history or binding
encodings, bump the version. The carrier preserves conversation history and
exact account/model replay scope, CarryBinding, named state, runner and
profile, not tree seal outcomes. Version 13 distinguishes `portable_session` from
the reserved `host_upgrade` kind; ordinary import accepts only portable
sessions (chapter 02 §2.5.1).

The second exception is the runner job-retention endpoint: a runner daemon MUST
serve, and the next build's daemon MUST speak, the retention endpoint of its own
version and of the immediately previous version, so that jobs started by the
previous build survive a planned upgrade. The endpoint is a same-user,
same-machine private socket with a fixed verb set (lines, transfer, jobs
snapshot/watch, read/poll, write, stop, disown, ack); it never starts work,
performs workspace effects or replays effects. Any change to its verbs or
encodings bumps its version; the previous version's speaker is kept for one
bump only.

The third exception is the credential gateway protocol (chapter 01 §7.5): a
gateway MUST serve, and the next build's callers MUST speak, the gateway
protocol of its own version and of the immediately previous version, so that
a planned upgrade of either side keeps inference and account operations
working. Its endpoints are fixed and path-versioned; it relays one upstream
exchange per request and grants no session or store authority. It never
retries, translates or fails over; the pre-plaintext idle-socket replacement
rule (chapters 02 §4.7 and 04 §3.4) stays within that exchange. Changes to
endpoints, encodings or binding facts bump its version; the previous version's
speaker is kept for one bump only. The gateway serves v2 and v1. Only v1
reports the former path-based Codex and SuperGrok credential authority and
selection digests.
Upgrade the gateway first so the old host can keep speaking v1 and the new
host can use v2 authority when carrying its sessions.

The fourth exception is in-place product and tree store revisions. A build
can advance an older revision of its own generation by additive changes only,
after exclusive owner admission, a consistent copy, and exact schema validation.
Neither store reads the other; each moves in a single transaction, and older builds refuse
newer revisions without writing. The first real revision can ship only when
no build that predates revision admission can run against that key; those
builds may write before their version checks. Generation changes discard the
revision steps and require offline migration (chapter 02 §2.5.1).

The runtime has a one-time, same-key exception to rebuild the object table
into epoch-neutral exact DDL. This is not an additive product or tree revision.
It requires a stopped store, exclusive SQLite ownership and a synced backup;
one transaction changes the shape without rewriting encoded payloads or
retained authority (chapter 01 §5.2).

Retention-only authority is irreversible for a host/line during that worker's
lifetime. Downgrading or reinstalling the drainer's own build triple does not
restore native control of that line: native adoption refuses until its retained
jobs are finished and consumed or explicitly disowned through a compatible
retention endpoint. Unrelated native lines remain usable. This exception grants
no cross-build native-store reader and changes no runtime schema or state key.

## How to read the rest of these docs

The [spec index](README.md) maps current behavior; the
[user guide](../user-guide/) owns usage. Read the section that owns the change.

### Spec admission

Change spec text only when a rule changes or the text is wrong. Replace the
text in its owning section rather than append to it; keep no decision history.
Do not add values that live in code, strings, inventories, private mechanisms,
test descriptions, or tool mechanics. Refer to the owning code rather than
copy its values.

Changes to public semantics, crate boundaries, persisted or wire formats,
security boundaries, or supported platforms MUST update the owning section
in the same change. Update the user guide in that change too if what an
operator types or sees changes.

Normative statements are present tense. **MUST**, **MUST NOT**, **SHOULD**, and
**MAY** appear only where omission would change safety, portability, or a
claimed contract.
