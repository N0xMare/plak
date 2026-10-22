# Getting started

Plak installs from a GitHub Release, from crates.io, or from a source checkout.
There is no `plak update`: updating means installing again.

## Install

The installer supports `aarch64-apple-darwin` and `x86_64-unknown-linux-musl`:

```sh
curl -fsSL https://raw.githubusercontent.com/N0xMare/plak/main/install.sh | sh
```

It downloads the latest release tarball and `SHA256SUMS` from GitHub Releases
(set `PLAK_VERSION` to pick a release), refuses a checksum mismatch, and checks
the build attestation when `gh` is installed and signed in. It installs `plak`,
`plakous-js-host` and `monty` into the same snapshot layout as `just install`
below, with links in `~/.local/bin`, and never needs `sudo`. Any other platform
is refused; build from source there.

With a Rust 1.99.0 toolchain, Cargo installs the same three executables from
crates.io into `~/.cargo/bin`, where Plak finds its workers beside itself:

```sh
cargo install --locked plak plakous-js-runtime plakous-py-worker
```

## Build from source

### Before you start

- **Rust 1.99.0**, pinned by `rust-toolchain.toml`, so rustup installs it on the
  first build. That is also the MSRV.
- **[`just`](https://github.com/casey/just)**, to run `just install` and the
  other recipes in the `Justfile`. Without it, use the Cargo commands below.
- A platform C/C++ compiler and linker for native dependencies such as SQLite
  and QuickJS. Cargo builds these dependencies; no Python interpreter is needed
  for ordinary Cargo builds or to run Plak. `just install` and the maintainer
  build recipes additionally require Python 3.11+ and `nm` (or `llvm-nm`) for
  linkage checks.
- Supported targets: `x86_64-unknown-linux-musl` and `aarch64-apple-darwin`.
  Intel macOS is not supported — process tools and Code Mode fail closed there.
  Anything else may compile, and compiling is not support.
- `grep`, `rg` or `find` on `PATH` for Claude sessions: the Claude tool set is
  Claude Code's native `Bash`, `Read`, `Edit`, `Write` and `TaskStop`, with no
  search tool, so the model searches through the shell.
- Session stores need access to the operating system's boot and clock-domain
  identity. Restricted access fails closed; see the
  [store contract](../spec/01-plakous-library.md#51-the-store-port).
- To run the test gate: `just dev-tools install` puts the pinned cargo-nextest
  and cargo-deny into the repository-local `.dev-tools/bin` and prefetches the
  locked public compile-contract dependencies. Installation needs network
  access; subsequent compile-contract gates stay offline.

### Build

```sh
git clone https://github.com/N0xMare/plak.git
cd plak
just install
```

`just install` builds a plain release `plak` with its `plakous-js-host` and
`monty` workers, checks their linkage, and installs them as an immutable
snapshot under `~/.local/lib/plak/release` (workers under the shared
`~/.local/lib/plak/sidecars`) with links `plak`, `plakous-js-host`, and `monty`
in `~/.local/bin`. Put `~/.local/bin` on your PATH. It refuses to replace
non-Plak files or links there, and refuses to run from a linked Git worktree.
Rebuilding never rewrites an executable a running session uses.

Without `just`, build the same executables with Cargo:

```sh
cargo build --release --locked -p plak
cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker
```

These separate Cargo resolutions produce `plak`, `plakous-js-host`, and the
`monty` Python worker in `target/release/`. Bare `cargo build` selects only
`plak`; do not build it and the Python worker in one invocation, because Cargo
unifies interpreter features into the parent. Run `target/release/plak` directly.
Keep the workers beside `plak` when copying the executables to another directory.
Cargo's standard `--target`, `--target-dir`, and `CARGO_TARGET_DIR` options
apply to all three artifacts.

For the bare `plak` commands below after a Cargo build, add the build directory
to this shell's PATH:

```sh
export PATH="$PWD/target/release:$PATH"
```

Cargo defaults to the installed host target. For the supported Linux musl build,
install Rust's `x86_64-unknown-linux-musl` target and a musl C toolchain, then use:

```sh
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
  cargo build --release --locked -p plak --target x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
  cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker \
    --target x86_64-unknown-linux-musl
```

The three binaries are then under `target/x86_64-unknown-linux-musl/release/`.
Use the corresponding compiler path if your musl toolchain has a different name.
Rustc finds its own `rust-lld`; this links static PIE. Nix's `musl-gcc` wrapper
is for C dependencies, not linking Rust static PIE (its specs inject a dynamic
loader). For per-install build-and-test evidence, run `just musl-native`;
`just musl-check` (maintainer-only: it needs the unpublished pinned container
image, spec 06 §6.1) remains the full pinned-container qualification.

For maintainer development, `just build-release` builds with the
`maintainer` product feature: maintainer-only verbs, always-on file logging, and
immutable snapshots under `~/.local/lib/plak/maintainer`. It installs the same
three links in `~/.local/bin` as `just install`, with the same refusals.
`just build` does the same into `target/debug/`; the debug binary is large and
its cold start is visible in the TUI.

```sh
$ plak version
plak 0.0.2 (release; binary 0123456789ab)
```

The binary ID is the executable identity used by `/reload` and handoffs.

To update later, pull the source and repeat `just install` (or the Cargo
commands). A running TUI shows `new build installed · /reload` once a new
`just install` snapshot is in place. Default builds
include the Git CLI backend only; release tarballs also omit JJ. Add
`--features plak/jj` to enable native JJ alongside Git (maintainer builds
already enable it). To build only native JJ:

```sh
cargo build --locked --release -p plak --no-default-features --features plak/jj
cargo build --locked --release -p plakous-js-runtime -p plakous-py-worker
```

Use `plak/git-cli` in place of `plak/jj` for a Git-only build. The root
workspace's default member is only `plak`; build both workers separately. Native
JJ workspace operations do not require the `jj` executable; the Git CLI backend
requires `git`.

## Connect an account

Plak supports three subscription accounts and four separately billed API accounts. See [providers and accounts](providers-and-accounts.md) for their exact names and enrolled models.

```sh
plak login codex     # ChatGPT / Codex subscription
plak login grok      # SuperGrok (also answers to `supergrok`)
plak login claude    # Claude subscription
plak key set openrouter
```

`plak login` opens a browser on **this** machine. Codex waits for its loopback
callback by itself — do not paste the localhost URL back at it. Grok and Claude
show you a code on the callback page and ask you to paste it in.

Over SSH or on a headless box use `plak login <account> --remote`: Plak prints
the URL and takes the pasted code. An SSH session refuses a local login and tells
you to add `--remote` rather than switching mode silently.

`plak key set` prompts for the key and writes it to `~/.plak/secrets/` with
mode `0600`. Environment variables (`OPENAI_API_KEY`, `XAI_API_KEY`,
`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`) override the files, and Plak never
loads a `.env` file.

Both are CLI-only; the TUI's Providers tab shows status and names the command.

## Check the setup

`plak status` reads local credential state only — no provider request, no
secrets printed. An account that cannot be read is a Setup needed row with the
repair, and the rest of the table still prints:

```
$ plak status
Ready

PROVIDER            KIND     PROFILE   CREDENTIAL
Claude Subscription OAuth    Accepted  Ready
Codex Subscription  OAuth    Accepted  Ready
Grok Subscription   OAuth    Accepted  Ready
OpenRouter API      API key  —         Key file

Setup needed

PROVIDER            KIND     PROFILE   CREDENTIAL
Anthropic API       API key  —         Missing
OpenAI API          API key  —         Missing
xAI API             API key  —         Missing
Local credential state only; no provider request was made.
```

`plak doctor` checks everything else — Plak home, databases, sidecar, config
validity, each OAuth credential — with a FIX column for anything wrong:

```
$ plak doctor
Plak doctor

CHECK              RESULT   DETAIL                                       FIX
Version            OK       available · 0.0.2                            —
Home               OK       available · /home/you/.plak                  —
Config             OK       valid · /home/you/.plak/config.toml          —
Product db         OK       available · /home/you/.plak/state/r25.e29.p33.t8/product.sqlite3 —
Composition host   OK       included · not required by the selected …    —
Oauth codex        OK       ready · generation 13 · record 22h old …     —

Result: 20 passed · 0 warnings · 0 failures
Provider credentials: run `plak status`
```

`doctor` always emits the same 21 checks. It exits 5 if any row is ERROR. Warnings alone do not fail it.

## Your first run

`plak run` runs non-interactively until idle, including continuation turns,
bounded by `--max-turns` (default 25). Shell `&` leftovers become jobs but do not
hold `run` open; they are stopped at exit. `--leave-jobs-running` leaves only
those jobs alive and unowned for an external verifier or CI caller, which must
clean them up. Redirect service output to a file or `/dev/null`. Budget-promoted
commands hold the run open while their shell leaders remain alive. Each turn can be billable. Name an exact model id — there is no
inference and no fuzzy matching — and, for a reasoning model, an effort:

```sh
plak run --account codex --model gpt-5.6-luna --effort low -- 'summarise src/'
```

`plak models codex` lists exactly which ids that account accepts. `--effort` is
**required** for a reasoning model and **refused** for the rest; there is no
default, the catalog row decides.

> Agents run with full coding tools and no approval gates, in your current
> directory. Process tools are worker-isolated, not workspace-isolated. If you
> want a real boundary, run Plak in a container or a VM.

## Your first TUI session

```sh
plak
```

A new session in the current directory; it needs an interactive terminal. Type a
prompt and press `Enter`. From there:

- `/` opens the slash palette (23 commands, including `/ps`).
- `Ctrl+X` reopens Menu; `?` opens While typing; `Enter` opens All keybinds. `/help` also prints the full sheet.
- `/agents` opens the Agents tab, where you choose a profile. On a fresh home,
  pick one and **Use now**; the agent attaches in the same session. An
  OpenRouter or direct API account needs a user template and a profile that
  names it, or the
  `--account`/`--model`/`--effort` flags of `plak run` and `plak agent`.
- `/settings` opens the settings tab; changes apply as you make them.
- `Ctrl+T` opens the last Activity tab (Processes or Agents).
- `Ctrl+Q`, or `Ctrl+C` twice, quits.

`plak resume` reopens a session from this directory via a picker;
`plak resume <uuid>` reopens one by id. See [the TUI guide](tui.md).

## Where things live

| Path | What |
| --- | --- |
| `~/.plak/` | Plak home, mode `0700`. Override with an absolute `$PLAK_HOME`. |
| `~/.plak/config.toml` | Product configuration |
| `~/.plak/agents/<name>.toml` | One agent template per file |
| `~/.plak/secrets/<account>.key` | API keys, mode `0600` |
| `~/.plak/references/<uuid>.json` | Imported archive retained after successful import; removed by session delete |
| `~/.plak/state/<format key>/` | This build's state; another build's state sits beside it untouched |
| `~/.plak/state/<format key>/sessions/<uuid>.events.jsonl` | Agent-readable session event logs |
| `~/.plak/state/<format key>/product.sqlite3`, `runtime.sqlite3` | Product and runtime state |
| `~/.plak/state/<format key>/jobs/<session>/<branch>-p<N>-<service>.log` | Background process output, mode `0600` (`0400` once the job was reported); the model's file-read tool may read exactly the paths its own process tools printed in the same Plak process; removed 24 h after the job was reported, when all such files would exceed 1 GiB (reported files first, then files untouched for 7 days, oldest first; new jobs get no file when the rest alone would exceed it), after 7 days untouched, or by session delete |
| `~/.plak/telemetry/telemetry-v<schema>.sqlite3` | Local analytics, isolated per schema; nothing leaves the machine |
| `~/.plak/logs/plak.log` | Log file, written when `PLAK_LOG` is set, or always in a maintainer build |
| `<project>/.plak/agents/` | Project templates; they win over user templates of the same name |
| `$XDG_STATE_HOME/plak/subscriptions/` | OAuth credentials and catalogs, mode `0700` (falls back to `~/.local/state`) |

OAuth credentials deliberately do **not** live in `~/.plak`: wiping your Plak
home does not log you out.

## The config in one glance

`config.toml` names a root template and a working set; each agent is its own
file:

```toml
# ~/.plak/config.toml
[root]
agent = "hub"

[roster]
working_set = ["hub", "implement-grok", "review-claude"]

[orchestration]
subagents = true
```

```toml
# ~/.plak/agents/hub.toml
account = "codex"
model = "gpt-5.6-luna"
effort = "low"
description = "Codex Luna hub"
# Only where hub runs as a child; the root spawns the working set.
spawns = ["implement-grok", "review-claude"]
```

`plak config show` prints the resolved config with `config_path=<file>` on its
first line, `plak config edit` opens it in `$EDITOR`, and `plak config init`
creates it if missing. `[roster]` and `[orchestration]` are config-only.

Every key is documented in [configuration](configuration.md). Complete,
tested example homes you can copy live in [`configs/`](../../configs/).

Next: [the CLI reference](cli.md), [the TUI](tui.md), or [agents and
orchestration](agents-and-orchestration.md).
