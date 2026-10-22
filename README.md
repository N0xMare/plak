# Plak

**You can have your cake and eat it too, but you must become the chef.**

## Overview

This repo is my take on the optimal agent harness playground implemented as a bunch of libraries, then an agent CLI/TUI application for primarily software engineering/computer use tasks on top. Plakous defines the harness library ecosystem. Plak is the CLI/TUI application I built on Plakous APIs.

The Plakous architecture target is one neutral runtime that owns state, tools, effects, durability, and recovery. All model-facing behavior is defined by versioned harness profiles, which are then compiled into a harness environment specifically made for that model/model family. The three most common inference protocol behaviors across the ecosystem are implemented as shared protocol kernels instead of reducing every backend/provider integration into a common minimal interface that loses valuable information specific to each protocol.

Plakous currently implements:
- Responses (e.g. Codex/OpenAI, Grok/xAI, several others)
- Messages (e.g. Claude/Anthropic)
- Chat Completions (e.g. OpenAI-compatible APIs and OpenRouter)

All three kernels are first class. Provider-specific dialects build on them while preserving the details and wire semantics of each API.

From this foundation, Plakous defines provider-specific defaults around what I am calling **minimum optimal co-design**: a balance between minimal harnesses like [pi](https://github.com/earendil-works/pi) and the specialized tools/grammar of native harnesses like Codex and Claude Code. My thesis is that retaining the model-facing details that matter, without carrying every native product feature, offers a better balance of capability, efficiency, and cost. To add to this, it's all fully open/customizable through configuration so custom harness environments and evals to compare them can all be implemented on top of plakous abstractions. Plakous core focuses on single-agent only, no orchestration/environment logic. Higher up the stack such as workspace/orchestration systems live in libraries above the single agent facade defined by core.

The Plak architecture builds on top of Plakous APIs adding the ability to use one or many Plakous agents in long/complex software engineering & computer use tasks, through CLI workflows (`plak run` and `plak agent`) or an interactive TUI (`plak`).

## Project status

Plak is a rapidly changing pre-release prototype, **not a dependable, hardened coding-agent product**, yet. My goal for the first release is a drop-in alternative to other CLI/TUI coding agents, with subscription and API-key access across these providers:

| Provider | Current Plak session route | Direct API-key account |
| --- | --- | --- |
| OpenAI | Codex subscription OAuth (`codex`) | `openai` |
| xAI | Grok subscription OAuth (`grok`) | `xai-api` |
| Anthropic | Claude subscription OAuth (`claude`) | `anthropic` |
| OpenRouter | API key (`openrouter`) | `openrouter` |

The session accounts are `codex`, `grok`, `claude`, `openrouter`, `openai`, `anthropic`, and `xai-api`. Direct API accounts use separate credentials and billing. See [providers and accounts](docs/user-guide/providers-and-accounts.md) for enrolled models, effort levels, routing and configuration limits.

**Claude subscription login disclosure:** Claude OAuth remains a first-class Plak model
route. Use at your own risk.

A supported route is not a promise that every model, effort level, or usage scenario works reliably. Testing has observed model refusals, incorrect tool outputs, and provider streaming failures although in many cases its been reliable for me in exhaustive testing. The [live status page](tests/live/STATUS.md) records the current live qualification state.

Built-in compatibility pins carry review expiry dates: the Codex subscription profile is valid through 2026-11-21T23:59:59Z and the OpenRouter Chat dialect until 2026-11-21T00:00:00Z; the OpenAI and xAI Platform API reviews expire 2027-01-20 (UTC). `plak doctor` and `plak status` warn within 14 days of any compiled review expiry; publication dry runs and uploads require more than 21 days remaining. After its date a build refuses that route until a newer build renews the review; see [troubleshooting](docs/user-guide/troubleshooting.md) ("native compatibility profile compiled into this build is inactive").

## Using Plak

### Permissions and agentic tools

**By default, agents run with full coding tools and no tool-approval prompts in their host environment.** They can modify or delete files and execute commands. Process tools run with your full host authority on Linux and macOS alike; there is no OS sandbox. Use a container or VM if you need a hard boundary around your machine. See [known limits and security model](docs/user-guide/known-limits-and-security-model.md).

### Install

On Apple Silicon macOS or Linux x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/N0xMare/plak/main/install.sh | sh
```

The installer downloads the latest GitHub Release (or `PLAK_VERSION`), verifies
its SHA-256 checksum, and installs `plak`, `plakous-js-host` and `monty` under
`~/.local/lib/plak` with links in `~/.local/bin` (put that directory on your
PATH). It needs no `sudo`. Alternatively, with Rust 1.99.0:

```sh
cargo install --locked plak plakous-js-runtime plakous-py-worker
```

Changes per release are in [CHANGELOG.md](CHANGELOG.md).

### Build from source

From the repository root, with Rust **1.99.0**, a platform C/C++ toolchain,
[`just`](https://github.com/casey/just), Python 3.11+ and `nm` (or `llvm-nm`)
available, build and install the full Plak product and both evaluator workers:

```sh
just install
```

`just install` builds a plain release `plak`, `plakous-js-host`, and `monty`,
checks their linkage, and installs them as an immutable snapshot under
`~/.local/lib/plak/release` with links named `plak`, `plakous-js-host`, and
`monty` in `~/.local/bin` (put that directory on your PATH). Existing non-Plak
files or links are refused, and it refuses to run from a linked Git worktree.
Run it again after `git pull` to update; a running TUI then shows
`new build installed · /reload`.

Without `just`, build the same three executables with Cargo:

```sh
cargo build --release --locked -p plak
cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker
```

Build the parent and workers in separate Cargo invocations to keep Monty runtime
features out of `plak`. Bare `cargo build` selects only `plak`. The commands
produce `plak`, `plakous-js-host`, and `monty` in
`target/release/` (or the selected Cargo target directory). Run
`target/release/plak` directly. Maintainer builds (`just build`,
`just build-release`, `just build-maintainer`) add maintainer-only verbs and
always-on file logging; they install snapshots under `~/.local/lib/plak/maintainer`
with the same links. No separate JavaScript runtime or preinstalled Monty
worker is required. Plak discovers its workers beside the executable.

To use the bare `plak` commands below after a Cargo build (not needed after
`just install`), add that build directory to your current shell's PATH from the
repository root:

```sh
export PATH="$PWD/target/release:$PATH"
```

Use your target-specific output directory instead when building with `--target`.

Supported targets are Linux x86_64 musl and Apple Silicon macOS. Default
builds include the
in-process jj-lib backend and the Git CLI backend; only the latter requires
the `git` executable. To update, run `git pull` and repeat the build command.

To **embed Plakous as a library**, depend on `plakous` from crates.io or on a
source checkout by path, using Rust 1.99.0 or later and edition 2024. Defaults are empty: a Custom
model with Direct tools needs no interpreter or provider credentials.
`composition` adds consumer-owned JavaScript through the `plakous-js-host`
worker, with no credential, HTTP or Python machinery and no `[patch]` table.
Native Codex Code Mode is restricted to enrolled Codex/OpenAI targets; a Custom
model instead uses `LocalCompositionProfile::product_javascript_v1` with the
neutral Pi vocabulary and explicit extension evidence. These are library options, not different Plak executables.

```sh
cargo run --locked -p plakous --example durable_session --features sessions-sqlite
```

The consumer examples are `cancellation_shutdown`, `direct_custom_tool`,
`durable_session`, `provider_direct`, and `worker_js_sidecar`; they use the facade plus documented
utility crates, not repository test support. Keep a downstream lockfile.
SQLite stores are format-epoch bound; the session export document is the
cross-build carrier. See [embedding](docs/user-guide/embedding.md) for the
complete manifest, worker build and per-example commands, feature costs,
presentation choices and lifecycle; [build and support](docs/spec/06-build-and-support-surface.md)
for contracts; and [getting started](docs/user-guide/getting-started.md) for product setup.

### Connect a provider

Run the command for the account you want to use:

```sh
plak login codex
plak login grok
plak login claude
plak key set openrouter
```

OAuth login opens a browser; over SSH use `plak login <account> --remote`. API-key entry is interactive. Then inspect your setup and the account's model IDs:

```sh
plak status
plak doctor
plak models codex
```

`plak status` reads local credential state without making a provider request. OAuth credentials live separately from the Plak home; see [providers and accounts](docs/user-guide/providers-and-accounts.md) for storage, refresh, API-key environment variables, and composition requirements.

### Run a task

From the project directory:

```sh
plak run --account codex --model gpt-5.6-luna --effort low -- 'Summarize this repository without changing files.'
```

Use an exact model ID from `plak models <account>`. The catalog determines which effort values and tool compositions are accepted. `js-ptc` uses the built JavaScript sidecar; `py-ptc` uses the built Monty worker. Both workers are discovered beside the binary; `PLAK_JS_HOST` and `PLAK_MONTY_WORKER` provide explicit path overrides.

### Configuration

```sh
plak config init
plak config show
plak config edit
```

The default home is `~/.plak/`; set `PLAK_HOME` to an absolute path to use a separate home. Agent templates live in `~/.plak/agents/`, with project overrides in `.plak/agents/`.

For a simple Codex root, create `~/.plak/agents/main.toml`:

```toml
account = "codex"
model = "gpt-5.6-luna"
effort = "low"
```

Set the root in `~/.plak/config.toml`:

```toml
[root]
agent = "main"
```

You can then start the TUI with `plak`. To configure multiple agents, use the [orchestration guide](docs/user-guide/agents-and-orchestration.md) and the complete example homes in [`configs/`](configs/). The [configuration guide](docs/user-guide/configuration.md) explains each setting.

### Commands

| Command | Purpose |
| --- | --- |
| `plak` | Start an interactive TUI session in the current directory |
| `plak run --account … --model … --effort … -- 'task'` | Run one non-interactive task |
| `plak agent --account … --model … --effort …` | Start the long-lived JSONL host for programmatic control |
| `plak resume [<uuid>]` | Resume in the TUI, using a picker or an exact session ID |
| `plak run --session <uuid> -- 'followup'` | Continue an existing session from the CLI |
| `plak models <account>` | Inspect the account's models |
| `plak status` / `plak doctor` | Inspect credentials and diagnose setup |
| `plak help` | Show command help |

`plak agent` expects JSONL input; it is not the interactive TUI. See the [CLI reference](docs/user-guide/cli.md), [TUI guide](docs/user-guide/tui.md), and [JSONL/orchestration guide](docs/user-guide/agents-and-orchestration.md) for the full interfaces.

## Documentation

- **[User guide](docs/user-guide/README.md):** installation, accounts, configuration, CLI/TUI usage, orchestration, and troubleshooting.
- **[Behavioral specification](docs/spec/README.md):** authority for architecture, runtime behavior, security boundaries, persistence, and supported surfaces. Start with the [system overview](docs/spec/00-overview.md).
- **[Crate map](docs/spec/00-overview.md#crate-map):** entrypoints and the authoritative package map.
- **[Example configurations](configs/):** annotated reference settings and complete scenario homes.
- **[Changelog](CHANGELOG.md)**, **[contributing](CONTRIBUTING.md)** and **[security policy](SECURITY.md)**.

For local development, `just fast` lints and tests the packages you touched and their dependents, `just check` runs formatting, linting, structure checks, feature builds, the parallel and serial offline Rust test lanes, public compile contracts and Python tooling tests before each commit. `just full` adds the durability lane, rustdoc and the offline publish dry run. Live tests are explicit opt-in and may incur provider usage. See [testing and evidence](docs/spec/05-testing-and-evidence.md).

## Repository layout

| Path | Contents |
| --- | --- |
| [`bin/plak/`](bin/plak/) | Product CLI, TUI, configuration, and composition root |
| [`crates/`](crates/) | Plakous libraries grouped by responsibility; dependency layers are enforced separately |
| [`crates/plakous/`](crates/plakous/) | Public complete-workflow entrypoint |
| `crates/core/` | Provider-neutral vocabulary, tool contracts, and single-agent runtime |
| `crates/protocols/`, `crates/providers/` | Shared wire kernels and provider integrations |
| `crates/harness/`, `crates/runtimes/` | Prompts, coding tools, composition, and evaluator runtimes |
| `crates/hosting/`, `crates/orchestration/` | Hosted sessions, VCS workspaces, and recursive orchestration above the facade |
| [`tests/`](tests/) | Public-consumer compile fixtures and repository integration contracts |
| [`eval/`](eval/) | Opt-in Harbor / Terminal-Bench harness comparisons against vendor CLIs |
| [`scripts/`](scripts/), [`tools/`](tools/) | Maintainer entrypoints, build helpers, policy checks, and profiling |
| [`configs/`](configs/) | Reference, provider, and scenario configurations |
| [`docs/spec/`](docs/spec/) / [`docs/user-guide/`](docs/user-guide/) | Behavioral authority and usage documentation |
| [`Justfile`](Justfile) / [`.config/nextest.toml`](.config/nextest.toml) | Development recipes and test profiles; `just dev-tools install` provides the pinned cargo-nextest and cargo-deny the gates require |

## License

Plak and Plakous are licensed under the [MIT License](LICENSE).

Third-party material keeps its upstream license:

- `vendor/` holds patched or vendored upstream sources, each with its own
  `LICENSE` (and `NOTICE` or `PROVENANCE.md` where upstream has one): Pydantic's
  `monty-typeshed` crate (MIT; the bundled typeshed stubs are Apache-2.0) and OpenAI Codex
  prompt and code-mode protocol material (Apache-2.0).
- Excerpts reproduced from the OpenAI Codex CLI (Apache-2.0, Copyright 2025
  OpenAI; license and notice in `crates/harness/prompts/vendor/codex-prompt/`): the `apply_patch`
  grammar and description (`crates/harness/tool-files/src/codex_patch.rs`,
  `crates/harness/tool-files/src/text.rs`), the `exec_command` description
  (`crates/harness/tool-process/src/text.rs`), and the model catalog facts in
  `crates/hosting/hosted/src/agent/fixtures/codex-reference-catalog.json`.
- Tool schemas adapted from xAI's [Grok Build](https://github.com/xai-org/grok-build)
  1.0.46 (Apache-2.0; license text in `crates/harness/prompts/vendor/codex-prompt/LICENSE`):
  `crates/harness/tool-process/src/grok.rs`.
- Prompt text and tool descriptions adapted from
  [pi](https://github.com/earendil-works/pi) 1.1.0, Copyright (c) 2025 Mario
  Zechner, used under the MIT License (the same terms as [LICENSE](LICENSE)):
  `crates/harness/prompts/src/bases/pi.rs`, `crates/harness/tool-process/src/pi.rs`,
  `crates/harness/tool-process/src/text.rs` and the `PI_*_DESCRIPTION` texts in
  `crates/harness/tool-files/src/lib.rs`.
