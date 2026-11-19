# Getting started

plakd installs from a GitHub Release, from crates.io, or from a source checkout.
There is no `plakd update`: updating means installing again.

## Install

The installer supports `aarch64-apple-darwin` and `x86_64-unknown-linux-musl`:

```sh
curl -fsSL https://raw.githubusercontent.com/N0xMare/plak/main/install.sh | sh
```

It downloads the latest release tarball and `SHA256SUMS` from GitHub Releases
(set `PLAK_VERSION` to pick a release), refuses a checksum mismatch, and checks
the build attestation when `gh` is installed and signed in. It installs `plakd`,
`plakous-js-host` and `plakous-py-host` into the same snapshot layout as `just install release`
below, with links in `~/.local/bin`, and never needs `sudo`. Any other platform
is refused; a source build does not make it a supported target.

With a Rust 1.100.0 toolchain, Cargo installs the same three executables from
crates.io into `~/.cargo/bin`, where plakd finds its workers beside itself:

```sh
cargo install --locked plakd plakous-js-runtime plakous-py-worker
```

## Build from source

### Before you start

- **Rust 1.100.0**, pinned by `rust-toolchain.toml`, so rustup installs it on the
  first build. That is also the MSRV.
- **[`just`](https://github.com/casey/just)**, to run `just install release` and the
  other recipes in the `Justfile`. Without it, use the Cargo commands below.
- A platform C/C++ compiler and linker for native dependencies such as SQLite
  and QuickJS. Cargo builds these dependencies; no Python interpreter is needed
  for ordinary Cargo builds or to run plakd. `just install release` and the maintainer
  build recipes additionally require Python 3.11+ and `nm` (or `llvm-nm`) for
  linkage checks.
- Supported targets: `x86_64-unknown-linux-musl` and `aarch64-apple-darwin`.
  Intel macOS is not supported — process tools and Code Mode fail closed there.
  Anything else may compile, and compiling is not support.
- `grep`, `rg` or `find` on `PATH` for Claude sessions: the Claude tool set is
  Claude Code's native `Bash`, `Read`, `Edit`, `Write` and `TaskStop`, with no
  search tool, so the model searches through the shell. `Read` returns PNG,
  JPEG, GIF and WebP as images when the model can accept them. Codex adds
  `view_image` for one workspace image.
- Session stores need access to the operating system's boot and clock-domain
  identity. Restricted access fails closed; see the
  [store contract](../spec/01-plakous-library.md#51-the-store-port).
- To run the test gate: `just dev-tools install` puts the pinned cargo-nextest,
  cargo-deny and ruff into the repository-local `.dev-tools/bin`, adds the
  `wasm32-unknown-unknown` rustc target to the pinned toolchain (the gate
  compiles the portable crates for it; it is not a supported product target)
  and prefetches the locked public compile-contract dependencies. Installation
  needs network access; subsequent compile-contract gates stay offline. A
  checkout set up before the gate checked that target re-runs
  `just dev-tools install`; without the target the `builds` group fails and
  says so. The gate also needs jj: `PLAK_TEST_JJ` naming one, else `jj` on
  `PATH`, else the newest pinned jj that install puts under `.dev-tools/jj`;
  without one the gate stops before it runs anything.

### Build

```sh
git clone https://github.com/N0xMare/plak.git
cd plak
just install release
```

`just install release` builds a plain release `plakd` with its `plakous-js-host` and
`plakous-py-host` workers, checks their linkage, and installs them as an immutable
snapshot under `~/.local/lib/plak/release` (workers under the shared
`~/.local/lib/plak/sidecars`) with links `plakd`, `plakous-js-host`, and `plakous-py-host`
in `~/.local/bin`. Put `~/.local/bin` on your PATH. It refuses to replace
non-plakd files or links there, and refuses to run from a linked Git worktree.
It also refuses a dirty tree, including untracked, nonignored files, and any
setting of `RUSTFLAGS`, even an empty one. Commit or remove those changes and
unset `RUSTFLAGS` before building.
Rebuilding never rewrites an executable a running session uses.
Both this source recipe and `install.sh` use the same snapshot installer
inside `plakd`. An authenticated shipping artifact does not need Python on the
installing machine. The source recipe still uses Python for build-time checks.

Without `just`, build the same executables with Cargo:

```sh
cargo build --release --locked -p plakd
cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker
```

These separate Cargo resolutions produce `plakd`, `plakous-js-host`, and the
`plakous-py-host` Python worker in `target/release/`. Bare `cargo build` selects only
`plakd`; do not build it and the Python worker in one invocation, because Cargo
unifies interpreter features into the parent. Run `target/release/plakd` directly.
Keep the workers beside `plakd` when copying the executables to another directory.
Cargo's standard `--target`, `--target-dir`, and `CARGO_TARGET_DIR` options
apply to all three artifacts.

For the bare `plakd` commands below after a Cargo build, add the build directory
to this shell's PATH:

```sh
export PATH="$PWD/target/release:$PATH"
```

Cargo defaults to the installed host target. For the supported Linux musl build,
install Rust's `x86_64-unknown-linux-musl` target and a musl C toolchain, then use:

```sh
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
  cargo build --release --locked -p plakd --target x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
  cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker \
    --target x86_64-unknown-linux-musl
```

The three binaries are then under `target/x86_64-unknown-linux-musl/release/`.
Use the corresponding compiler path if your musl toolchain has a different name.
Rustc finds its own `rust-lld`; this links static PIE. Nix's `musl-gcc` wrapper
is for C dependencies, not linking Rust static PIE (its specs inject a dynamic
loader). For complete native musl platform evidence, run `just musl native`;
`just musl container` (maintainer-only: it needs the unpublished pinned container
image, spec 06 §6.1) remains the full pinned-container qualification.

For maintainer development, `just build release` and `just build debug` build
with the `maintainer` product feature (maintainer-only verbs and always-on file
logging) into `target/release/` and `target/debug/`. They do not install a
snapshot or change PATH. `just install maintainer` builds the dogfood profile
(release codegen, no LTO, incremental, line tables) and installs an immutable
snapshot under `~/.local/lib/plak/maintainer`, with the same three links in
`~/.local/bin` as `just install release` and the same refusals. The debug
binary is large and has a visible cold-start cost.

```sh
$ plakd version
plakd 0.0.4 (release; binary 0123456789ab)
```

The binary ID is the executable identity used by build checks and host upgrades.

To update later, pull the source and repeat `just install release` (or the Cargo
commands). Existing processes keep their immutable installed image until
restarted or upgraded through the host. Default builds
include the Git CLI backend only; release tarballs also omit JJ. Add
`--features plakd/jj` to enable native JJ alongside Git (maintainer builds
already enable it). To add native JJ to the default product:

```sh
cargo build --locked --release -p plakd --features plakd/jj
cargo build --locked --release -p plakous-js-runtime -p plakous-py-worker
```

Without that extra feature the default product is Git-only. The root
workspace's default member is only `plakd`; build both workers separately. Native
JJ workspace operations do not require the `jj` executable; the Git CLI backend
requires `git`.

## Uninstall

To uninstall an `install.sh` or `just install` installation, run
`sh install.sh uninstall` from a source checkout. This removes plakd-owned links
from `~/.local/bin` and `${CARGO_HOME:-~/.cargo}/bin` (including Cargo entries
relinked by `just install`) and the release, maintainer and shared sidecar roots
in `~/.local/lib/plak`. Preview these removals with
`sh install.sh uninstall --dry-run`. Unrelated links and files stay in place. It never deletes
the product home (`~/.plak` or `PLAK_HOME`) or credentials; there is no
home-deletion flag. For a crates.io installation, use
`cargo uninstall plakd plakous-js-runtime plakous-py-worker` instead.

## Connect an account

plakd supports three subscription accounts and four separately billed API accounts. See [providers and accounts](providers-and-accounts.md) for their exact names and enrolled models.

```sh
plakd login codex     # ChatGPT / Codex subscription
plakd login grok      # SuperGrok (also answers to `supergrok`)
plakd login claude    # Claude subscription
plakd key set openrouter
```

`plakd login` opens a browser on **this** machine. Codex waits for its loopback
callback by itself — do not paste the localhost URL back at it. Grok and Claude
show you a code on the callback page and ask you to paste it in.

Over SSH or on a headless box use `plakd login <account> --remote`. Codex prints
a device-login URL and code, then waits for authorization; Grok and Claude use
paste-back. See [remote login](providers-and-accounts.md#oauth).
An SSH session refuses a local login and tells
you to add `--remote` rather than switching mode silently.

`plakd key set` prompts for the key and writes it to `~/.plak/secrets/` with
mode `0600`. Environment variables (`OPENAI_API_KEY`, `XAI_API_KEY`,
`ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`) override the files, and plakd never
loads a `.env` file.

Host clients can also implement the confirmed Accounts operations for login
and masked key entry. Credential resolution remains on the host.

## Check the setup

`plakd status` reads local credential state only -- no provider request, no
secrets printed. An account that cannot be read is a Setup needed row with the
repair, and the rest of the table still prints:

```
$ plakd status
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

`plakd doctor` checks everything else -- plakd home, databases, sidecar, config
validity, each OAuth credential — with a FIX column for anything wrong:

```
$ plakd doctor
plakd doctor

CHECK              RESULT   DETAIL                                       FIX
Version            OK       available · 0.0.4                            —
Home               OK       available · /home/you/.plak                  —
Config             OK       valid · /home/you/.plak/config.toml          —
Product db         OK       available · /home/you/.plak/state/r28.e39.p39.t11/product.sqlite3 —
Composition host   OK       included · not required by the selected …    —
Oauth codex        OK       ready · generation 13 · record 22h old …     —

Result: 20 passed · 0 warnings · 0 failures
Provider credentials: run `plakd status`
```

The checks depend on the build, configuration and configured runners; the
example is illustrative. `doctor` exits 5 if any row is ERROR. Warnings alone
do not fail it.

## Your first run

`plakd run` runs non-interactively until idle, including continuation turns,
bounded by `--max-turns` (default 25). Shell `&` leftovers become jobs but do not
hold `run` open; they are stopped at exit. `--leave-jobs-running` leaves only
those jobs alive and unowned for an external verifier or CI caller, which must
clean them up. Redirect service output to a file or `/dev/null`. Budget-promoted
commands hold the run open while their shell leaders remain alive. Each turn can be billable. Name an exact model id — there is no
inference and no fuzzy matching — and, for a reasoning model, an effort:

```sh
plakd run --account codex --model gpt-5.6-luna --effort low -- 'summarise src/'
```

`plakd models codex` lists exactly which ids that account accepts. `--effort` is
**required** for a reasoning model and **refused** for the rest; there is no
default, the catalog row decides.

> Agents run with full coding tools and no approval gates, in your current
> directory. Process tools are worker-isolated, not workspace-isolated. If you
> want a real boundary, run plakd in a container or a VM.

## Connect your client

Start `plakd host`, then connect your own client through
`plakd host connect --stdio`. A missing host does not start automatically;
local startup requires the bridge's explicit `--auto-start` option. There is
no public terminal application, `resume` picker or bare interactive launch.
See [Integrating](integrating.md) for schema export and the wire contract.

## Where things live

| Path | What |
| --- | --- |
| `~/.plak/` | plakd home, mode `0700`. Override with an absolute `$PLAK_HOME`. |
| `~/.plak/config.toml` | Product configuration |
| `~/.plak/agents/<name>.toml` | One agent template per file |
| `~/.plak/secrets/<account>.key` | API keys, mode `0600` |
| `~/.plak/state/<format key>/` | This build's state; another build's state sits beside it untouched |
| `~/.plak/state/<format key>/sessions/<uuid>.events.jsonl` | Agent-readable session event logs |
| `~/.plak/state/<format key>/product.sqlite3`, `runtime.sqlite3` | Product and runtime state |
| `~/.plak/state/<format key>/jobs/<session>/<branch>-p<N>-<service>.log` | Background process output, mode `0600` (`0400` once the job was reported); the model's file-read tool may read exactly the paths its own process tools printed in the same plakd process; removed 24 h after the job was reported, when all such files would exceed 1 GiB (reported files first, then files untouched for 7 days, oldest first; new jobs get no file when the rest alone would exceed it), after 7 days untouched, or by session delete |
| `~/.plak/telemetry/telemetry-v<schema>.sqlite3` | Local analytics, isolated per schema; nothing leaves the machine |
| `~/.plak/logs/plak.log` | Log file, written when `PLAK_LOG` is set, or always in a maintainer build |
| `<project>/.plak/agents/` | Project templates; they win over user templates of the same name |
| `$XDG_STATE_HOME/plak/subscriptions/` | OAuth credentials and catalogs, mode `0700` (falls back to `~/.local/state`) |

OAuth credentials deliberately do **not** live in `~/.plak`: wiping your plakd
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

`plakd config show` prints the resolved config with `config_path=<file>` on its
first line, `plakd config edit` opens it in `$EDITOR`, and `plakd config init`
creates it if missing. `[roster]` and `[orchestration]` are config-only.

Every key is documented in [configuration](configuration.md). Complete,
tested example homes you can copy live in [`docs/reference-config/`](../reference-config/) and [`tests/fixtures/config-scenarios/`](../../tests/fixtures/config-scenarios/).

Next: [integrate your client](integrating.md), use the [CLI reference](cli.md),
or configure [agents and orchestration](agents-and-orchestration.md).
