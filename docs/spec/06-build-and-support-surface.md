# Build and support surface

This chapter states how the workspace is built and installed, which packages
carry a support promise, what may be depended on, and what the repository
distributes. Chapter 00 gives the crate map and the facade feature table; this
chapter gives the classification rules, the build recipes, and the policies that
constrain them.

## 1. Distribution

**Plak and Plakous release as one exact cohort.** Every workspace package has
the workspace version `0.0.1`; internal dependencies are declared once in the
root `[workspace.dependencies]` as `{ version = "0.0.1", path = ... }` and
inherited with `.workspace = true`, so each packaged manifest requires exactly
its siblings' version (Cargo's caret on a `0.0.x` version admits only that
version). The vendored `plakous-codex-code-mode-protocol` keeps its upstream
version and is required exactly.

A package's class decides publication (§3): supported and published-internal
packages, the vendored Code Mode protocol snapshot among them, are publishable
to crates.io; repository-only packages, the compile fixtures under
`tests/fixtures/`, and the `crates/core/model/fuzz` crate set `publish = false`.
`tools/checks/check_workspace.py` enforces both directions and the registry
metadata of §3.3. Uploading the cohort is a maintainer act outside the gate; a
source checkout remains a complete build. The `0.0.x` cohort carries no API,
wire, or persisted-format compatibility obligation across versions beyond the
export-document rule of chapter 02 §2.5.1.

Each `v<version>` tag also has a GitHub Release carrying one tarball per
supported target and `SHA256SUMS`, installed by the repository's `install.sh`
(§6.6). There is no `plak update` verb and no self-update path. Updating means
rerunning the installer or pulling the source and rebuilding (§6.3).

Support classes describe which surfaces a consumer is entitled to rely on, and
which inventories are pinned so that entitlement is checkable (§3).

The product's facade dependency enables the three direct API providers as well
as subscriptions and OpenRouter. Each provider feature exposes the same
header-free exchange observer through the facade; it grants no retry or
credential authority. The xAI TLS fixtures require Tokio's `io-util` only as a
dev dependency. No new runtime dependency or platform is introduced by the
API execution surface.

## 2. Platforms

The workspace is Rust edition 2024. MSRV is 1.99.0 and `rust-toolchain.toml`
pins channel `1.99.0` with the `minimal` profile plus `clippy` and `rustfmt`, so
the ordinary development build is the MSRV build and there is no separate MSRV
job. `clippy.toml` repeats the MSRV so lint suggestions stay inside it.
`.cargo/config.toml` sets `resolver.incompatible-rust-versions = "fallback"`.

Two targets are supported, and one near neighbour is explicitly not:

| Target | Status |
| --- | --- |
| `x86_64-unknown-linux-musl` | Supported target; musl evidence comes from an explicit `--target x86_64-unknown-linux-musl` build |
| `aarch64-apple-darwin` | Supported for daily maintainer builds and the product |
| `x86_64-apple-darwin` | Not supported. Process tools and Code Mode fail closed |

Anything else is unsupported: it may compile, and compiling is not support.

The ordinary release commands use the selected Cargo target (the host target
by default); a Linux build without `--target` produces GNU/Linux binaries, and
results from it establish GNU/Linux behavior only.

## 3. Package classes

Publication and Rust nameability confer accessibility, not support. A package
being present, a symbol being `pub`, and a crate being resolvable are facts about
reachability. None of them confers integration status, context evidence,
provider qualification, recovery certification, or source compatibility.

| Class | Meaning |
| --- | --- |
| Supported | A documented consumer product with an explicit inventory, a compatibility baseline, a direct external workflow, and a correctness obligation |
| Published internal | A necessary dependency and deliberate advanced-use implementation surface, released in the exact cohort, with no direct-use compatibility, qualification, or support promise |
| Repository-only | Test, fixture, and crash-harness packages kept outside the production dependency closure entirely and never published |

The workspace holds 40 packages: five supported, 32 published internal, three
repository-only.

The root `[workspace].exclude` includes `.plak`, the local operator-state and
fork-worktree directory. Cargo must not treat packages in that directory as
members of the enclosing workspace. The workspace checker requires this
exclusion and continues to reject other unreviewed nested-package exclusions.

**Supported (5)**

`plakous`, `plakous-agent`, `plakous-model`, `plakous-tools`, `plak`.

**Published internal (32)**

`plakous-host-protocol`, `plakous-workspace`, `plakous-recursive-subagents`,
`plakous-recursive-subagents-store`, `plakous-hosted`,
`plakous-anthropic`, `plakous-attempt`, `plakous-auth`,
`plakous-chat-completions`, `plakous-code-mode`, `plakous-codex-code-mode-protocol`,
`plakous-compose`, `plakous-exchange`,
`plakous-http`, `plakous-js-eval`, `plakous-mcp`, `plakous-js-runtime`, `plakous-py-worker`, `plakous-messages`, `plakous-openai`,
`plakous-openrouter`, `plakous-prompts`, `plakous-py-eval`,
`plakous-responses`, `plakous-responses-dialect`, `plakous-store`,
`plakous-tool-files`, `plakous-tool-plan`, `plakous-tool-process`,
`plakous-tool-search`, `plakous-tool-web`, `plakous-xai`.

**Repository-only (3)**

`plakous-test-support`, `plakous-offline-crash-harness`,
`plakous-mcp-fake-server`.

Every one of these declares its `layer` and `class` in `[package.metadata.plakous]`,
and `tools/checks/check_workspace.py` (via `tools/checks/plakous_policy.py`) checks each
against the workspace policy in `[workspace.metadata.plakous]` in the root
`Cargo.toml` — the layer list, the legal production edges, the directory
template and the legal classes per layer — so a package cannot be added, moved,
or reclassified silently.

Library directories group related responsibilities; architectural layers remain
explicit metadata. Directory placement never grants dependency permission.

| Directory | Contents | Layer |
| --- | --- | --- |
| `crates/core/` | model, tools, agent | core |
| `crates/common/` | attempt, http, auth | common |
| `crates/runtimes/` | js-eval, js-runtime, py-eval, py-worker, code-mode | engine |
| `crates/protocols/` | Responses, its shared dialect, Messages, Chat | protocol |
| `crates/providers/` | OpenAI, Anthropic, xAI, OpenRouter | provider |
| `crates/store/` | session-store implementations | store |
| `crates/harness/` | prompts, compose, coding tool packs | harness |
| `crates/plakous/` | complete-workflow public entrypoint | facade |
| `crates/hosting/` | host protocol and hosted runtime | host |
| `crates/orchestration/` | workspace, recursive subagents and their store | host |
| `bin/plak/` | CLI, TUI and JSONL product | bin |
| `tests/` | repository support, fixtures and harnesses | test |
| `vendor/` | the reviewed Code Mode protocol snapshot (`vendor/monty-typeshed` is a `[patch]` source, not a member) | vendor |
| `eval/` | Opt-in, paid Harbor / Terminal-Bench harness bakeoff; linux/amd64 or linux/arm64 GNU agent packaging selected by `HARBOR_ARCH`, not musl qualification; ARM cells are rebuilt from task definitions rather than the published amd64 images; an eval lane is not product support; paid runs are not in the daily gate | (none) |

The `harness` layer sits under the facade and depends on `core`, `common`,
`engine`, and itself: a composition crate may depend on a pack. One rule
narrows that — `plakous-prompts` depends on no pack and no composition crate in
production, so instruction text never acquires an opinion about which tools
exist. The `host` layer sits above the facade and consumes only `plakous` and
other `host` packages; nothing in it is re-exported by the facade. The
`vendor` layer holds `plakous-codex-code-mode-protocol` alone; it depends on
no workspace package and only `engine` may depend on it. It is a member so the
cohort packages together (§6.5), and it keeps its upstream edition,
rust-version, Apache-2.0 license and `unsafe_code = "forbid"` lint table rather
than the workspace's.

`plak` names `plakous-host-protocol` directly under an edge exception recorded
in the same policy table, and `[workspace.metadata.plakous.modules.plak.tui]`
constrains what the binary's `tui` module may name.

### 3.1 What each supported package promises

Support is limited to five inventories, and each is pinned by code rather than
by whatever happens to be `pub`.

| Package | Supported inventory | Pinned by |
| --- | --- | --- |
| `plakous` | The curated root: `VERSION`, plus every import and workflow in the facade fixture, including the explicitly named model, tool, coding-pack, registry, and workspace values, and the opaque session workflows | The external `tests/fixtures/facade-consumer`, the one list of the supported root: the contracts gate builds and runs it, so every name compiles from outside the workspace and every workflow in it executes |
| `plakous-agent` | The explicit construction, prompt/turn, control and acknowledgement, result, observation and presentation, context-source, profile and policy, usage, and telemetry reexports. No raw checkpoint, store, or runtime authority | `crates/core/agent/src/lib.rs` and `tests/fixtures/agent-consumer` |
| `plakous-model` | The Custom-model construction and registration seam: `LanguageModel`, validated preparation inputs and results, Custom descriptor and implementation identity, the identifiers and capability values needed to build them, and the typed preparation error | `tests/fixtures/model-consumer` |
| `plakous-tools` | The Custom-tool construction and registration seam: executor, future and context, invocation and outcome, registry builder and frozen registry, registration and executor identifiers, plus the model-owned definition, schema, effect, and concurrency values | `tests/fixtures/tools-consumer` |
| `plak` | The installed process behaviors of the binary: version, help and recursive help; `config init/set/show`; `doctor`, `session list`, key status; confirmation and no-guessed-context failures; non-TTY bare invocation; missing-auth exit 3; malformed-argv exit 2. There is no supported direct Rust library API | `bin/plak/tests/integration/cli_contract.rs`. `cli.rs` is implementation grammar, not output or exit authority |

A direct-package symbol outside the corresponding fixture is nameable, not
supported — even when a facade reexport of the same name is separately
supported through the curated root. Root-facade support and direct-package
support are curated independently.

### 3.2 Withheld authority

The facade's always-on `application` executor and `run_once_until_signal` take
caller-supplied cancellation and contain no terminal policy. Ctrl-C, secret
reading and browser opening are Plak implementations. The neutral
`sessions::isolated_tool_scheduler` supports concurrent sessions in one
process without exposing scheduler authority. Assignment prompt renderers
and their unchanged digest pins live in `plakous-hosted::child_prompts`;
no multi-agent prompt module is exported by the L0 facade.

Some items must be `pub` for cross-crate composition inside an unpublished
source tree. They are not consumer surface:

- the `plakous_agent::internal` tier — the single-owner driver, private
  endpoint, durable coordinator, checkpoint/manifest/lifecycle authority, object
  graph and codecs, recovery-scope table, rehydration,
  runtime ledgers, and the runtime-only tool scheduling, authorization, effect,
  timer, task, and supervision values;
- `plakous_tools::internal`, a construction-only seam that grants no scheduling,
  admitted-effect, or supervision authority;
- `plakous-store` mechanics — the semantic store trait, commands, leases, CAS,
  receipts, backend errors, SQLite schema, and rows;
- any facade bridge returning either authority family; and
- fixture, crash-harness, campaign, pricing, approval, reservation, settlement,
  anti-rollback, and maintainer-liability policy.

The internal tier is deliberately nameable so official sibling crates can
compose against it. `#[doc(hidden)]` is not a support boundary, and the lasting
proof is the supported-root inventory plus the compile-negative pins described
in chapter 05.

### 3.3 Package contents

Every non-repository-only package builds from its own packaged files, so
publication needs no file outside the package root:

- Every `include_str!`/`include_bytes!` in a package's tracked Rust files,
  tests included, resolves inside that package. Embedded assets live in the
  package that embeds them: the built-in profile homes in `bin/plak/presets/`,
  the vendored Codex instruction text in
  `crates/harness/prompts/vendor/codex-prompt/`, and test fixtures beside the
  tests that read them (a fixture two packages read is copied into each).
- No packaged manifest names a repository-only package outside
  dev-dependencies, and a dev-dependency on one is path-only, so `cargo
  package` strips it.
- `plak/test-hooks` is repository-only. Cargo packages no regular dependency
  that is not on the registry, optional or not, so the repository manifest
  keeps the feature with its optional `plakous-offline-crash-harness` edge and
  the published manifest is that manifest with both removed
  (`tools/release/publish_shape.py`; `stage DEST` copies the tree with that
  manifest for `cargo package`). Every `cfg(feature = "test-hooks")` site
  compiles out of the published crate, and the workspace lints declare the
  feature name to `check-cfg`, so it builds without an unexpected-cfg warning.
  Repository builds, the gate, and the live drivers keep
  `--features plak/test-hooks`.
- The Code Mode grammar snapshot under `vendor/codex-code-mode-protocol` is the
  package `plakous-codex-code-mode-protocol`: upstream `codex-code-mode-protocol`
  renamed, keeping the upstream version, `LICENSE`, `NOTICE` and
  `PROVENANCE.md`.

- A published package carries the registry metadata: its description,
  `license`, `repository` and `homepage` (both the repository URL),
  `documentation` (`https://docs.rs/<name>` for a library; `plak` and the
  binary-only `plakous-py-worker` point into the repository), a `readme`, one
  to five crates.io keywords and categories, and a `LICENSE` file in the
  package root. Members other than the vendor snapshot are MIT, and each
  `LICENSE` is a link to the root `LICENSE`, which Cargo packages as a regular
  file; the vendor snapshot keeps its Apache-2.0 `LICENSE` and `NOTICE`. `plak` and `plakous` use the root
  `README.md`; every other package has its own short `README.md`.
- Members other than the vendor snapshot inherit `[workspace.package].exclude`: `/tests/`, `/benches/` and
  `/fixtures/` stay in the repository. A packaged crate builds its library and
  binaries; its integration tests, benches and test data, and the unit tests
  that read that data, run only from the repository.

`tools/checks/check_workspace.py` enforces these rules and judges `plak` by its
published shape.

## 4. Product features

Compile-time capabilities are additive. A capability merits a feature when it
adds at least five exclusive packages, a C/system build or 1 MB stripped; ships
or spawns an executable; has a distinct trust, legal or network posture; or
integrates with a platform. Other choices belong in runtime configuration.
Feature selection must not change persisted or wire vocabularies. The release
profile remains speed-focused; feature work does not change its optimization.

The hosted `RuntimeFactory` is a sealed host-level construction port, not an
embedder extension point despite the published-internal crate's nameability.
Only production construction is available without `test-support`; the cfg'd
test kit supplies the scripted implementation and the private registered-route
authority token. Native and API rebind target opens use that same sealed port.

The workspace checker enforces configuration placement through
`[workspace.metadata.plakous.cfg.<package>]`. `feature-sites` defaults to
`src/lib.rs` and `src/registry.rs`; these allow gated static-entry references
(`&module::ENTRY` or `&ENTRY`), not arbitrary gated items or statements. Other feature
predicates must gate a module declaration (including disabled-module swaps).
`env-sites` defaults to
empty and lists reviewed environment-reading boundary files. Plak's boundary
files are `environment.rs`, `home/paths.rs` and `home/secrets.rs`; the tools
child-environment builder and workspace VCS environment files are reviewed
library exceptions. Imported environment readers, including aliases and
function items, are inventoried as well as direct calls.

Test-seam features (`test-support`, `test-hooks`, `commit-trace`,
`fault-injection`, `perf-instrumentation`, `scripted-code-mode-enrolment`)
belong only on test-support module declarations in libraries and in Plak's
`lib.rs`/`test_hooks.rs` composition root. Production control-flow seams are
existing debt, not a precedent for new sites. The checker counts feature,
test-seam and environment placement debt separately, with exact current
`feature-ceiling`, `test-seam-ceiling` and `env-ceiling` inventories and a
review reason. Missing ceilings mean zero; growth fails and reductions require
lowering the ceiling in the same change. Test-only items and test source
trees are excluded; `cfg(any(test, feature = ...))` still ships under the
feature and is counted. Comments and literals are not configuration sites.

`plakous-hosted::capabilities` owns the complete current route metadata in
per-route modules and one ordered registry. Plak's private product registry
adds its labels, recommendation aliases, doctor probe and bundled preset for
each hosted identity. No new Cargo features or feature implications are
introduced by that registry; the default and maintainer closures and the
speed-focused release profile are unchanged. Runtime construction and its
test-support seams remain separate from this metadata registry. Serialized
binding enums, the complete template-account vocabulary and credential-store
directory names remain ungated; they describe records, not enabled menus.

`plakous` facade features are listed in chapter 00. The host and product
features and the evaluator selection are summarized here; backend defaults are
called out explicitly.

| Crate | Feature | Effect |
| --- | --- | --- |
| `plak` | `maintainer` | Empty feature gating only Plak's own cfg sites: maintainer verbs, the offline reload rehearsal and its test target, the explicit processing-tier selection, and automatic private inference capture (§6.2) |
| `plak` | `otel` | OTLP export and the typed telemetry bridge. Default builds carry no `opentelemetry` dependency |
| `plak` | `test-support` | Isolated product fixtures and the internal rendering benchmark entry; required by `telemetry_contract`, whose target is skipped without it |
| `plakous` | `composition` | Code Mode presentation and JavaScript evaluation through the `plakous-js-host` sidecar. This is the whole evaluator capability; it links no credential, HTTP, subscription, or Python machinery |
| `plakous` | `composition-python` | The Monty-backed Python PTC projection over the same attach path; implies `composition` |
| `plakous-compose` | `scripted-code-mode-enrolment` | One extra enrolled local Code Mode target whose every field is a `scripted`/`offline` identifier, so the repository's offline harness can compile Code Mode against a scripted model. Off by default, reached only through the harness, and it leaves the enrolment gate's own conditions unchanged. See chapter 05 §2.6 |
| `plakous-http` | `fault-injection` | `PLAK_TEST_FAULTS` transport fault injection (spec 05 §2.8). Enabled only by the repository harness crate, so `plak/test-hooks` is its one product edge; `check_features.py` refuses it in a default `plak` |
| `plakous-store` | `commit-trace` | Repository-only process-wide durable SQLite commit tracing and indexed `SIGKILL` cuts (spec 05 §2.6, commit trace contract). Enables `sqlite`, `rusqlite/hooks`, `nix/signal`, and optional JSON encoding. Inert without `PLAK_TEST_COMMIT_TRACE`; reached by Plak through the offline-crash harness under `test-hooks` and through its harness dev-dependency in test builds, never a default or maintainer/release product build. The dev edge gives runtime-only tracing without `test-hooks`; a sweep must require `cfg(feature="test-hooks")` for all three stores. |
| `plakous-recursive-subagents-store` | `commit-trace` | Optional direct `plakous-store` edge solely to share that process-wide tracer. This explicit test-only host-to-store exception is pinned by the workspace checker; `plak/test-hooks` directly forwards it over the existing product-to-tree dependency, avoiding a harness-to-tree dev-dependency cycle. No facade feature or API is added. Feature checks and tooling tests prove the edge absent from the tree's default direct dependencies and both tracing features absent from default and maintainer/release Plak closures. |
| `plakous-openai` | `test-support` | Implies `native-auth`; exposes the doc-hidden builder that binds Codex browser-login callbacks on an ephemeral port instead of the pinned redirect ports. A dev-dependency of `plakous`, never a production edge |
| `plakous-hosted` | `test-support` | Test-only session doubles, host-loop drivers, and scripted `RuntimeFactory` registrations; the construction port itself is always available. A dev-dependency of `plak`, never a production edge |
| `plakous-host-protocol` | `test-support` | Test-only view constructors; a dev-dependency of `plakous-hosted` and `plak`, never a production edge |
| `plak` | `git-cli`, `jj` | Defaults enable only `git-cli`. `jj` forwards the native `jj-lib` backend; `maintainer` enables `jj`. Release tarballs and `build-plak.sh --artifacts` use defaults, without JJ. A no-JJ build refuses JJ-only repositories with an instruction to use the `jj` feature; colocated repositories use Git copies, with a doctor row and one new-session notice. |
| `plakous-hosted`, `plakous-recursive-subagents`, `plakous-recursive-subagents-store`, `plakous-workspace` | `git-cli`, `jj-lib` | Explicit backend forwarding; library defaults include both. With defaults disabled, either or neither backend may be selected. Without JJ, colocated repositories use Git when enabled; JJ-only repositories refuse. |
| `plakous-workspace` | `test-support` | `FakeVcs` and the real-`git`/`jj` repository fixtures; a dev-dependency of `plakous-recursive-subagents`, never a production edge |
| `plakous-recursive-subagents` | `test-support` | `MemoryTreeStore`, the tree's bootstrap and mutation fixtures, and the hub's; forwards `plakous-host-protocol/test-support` and `plakous-workspace/test-support`, never a production edge |
| `plakous-recursive-subagents-store` | `test-support` | The store conformance suite's tree fixtures; never a production edge |

Neither `plakous` nor `plakous-hosted` exposes a `maintainer` feature.
The facade's unsupported doc-hidden offline inspector compiles under
`sessions-sqlite` (chapter 01 §12.1). Hosted root-restore and bounded child-page
presentation helpers are doc-hidden repository seams without a feature gate,
not supported embedder authority. Plak's maintainer feature enables its `jj` feature.

`plak` depends on exactly `plakous`, `plakous-hosted`,
`plakous-recursive-subagents`, `plakous-recursive-subagents-store`, and
`plakous-host-protocol` in production, and on no other package in the tree —
`plakous-workspace` in particular is reached only through the tree crate. The
product declares its required facade features explicitly in `bin/plak`:
default features off, `native-subscriptions`, `openai`, `anthropic`, `xai`,
`openrouter`, and `sessions-sqlite` on. Hosted libraries also declare the facade
features their own implementations require; Cargo unifies these requirements.
Every package must declare the dependency features its own Rust targets use;
features enabled only by another workspace member are not evidence that the
package builds on its own. The feature checker scans UUID generation
constructors in package source, tests, benches, examples and build scripts
(including inline test modules, excluding comments and literals), and requires
the matching `v1` through `v8` feature in that package's UUID dependency
declarations. Workspace-inherited features and local additions are additive.
In particular, the facade's dev-dependency and the offline-crash harness's
dependency explicitly enable `uuid/v4`; both support standalone
`cargo check -p <package> --all-targets --locked`.
`native-subscriptions` implies `composition` and `composition-python`, so the
product closure is unchanged by the split.

`plakous` carries ten public optional features. The reviewed closure cap in
`tools/checks/check_features.py` is **10**: the tenth is `mcp`, the MCP client,
which no other facade feature implies and only `full` and Plak's product
closure enable. `plakous-mcp` depends on `plakous-auth` with `http-refresh`
(the private credential store and claimed refresh, for MCP OAuth), so an
`mcp` selection reaches `plakous-auth` without exposing `plakous::auth`.
The cap bounds the per-package subset enumeration, so the
facade's own run is 1026 closures. Any eleventh public feature needs its own
reviewed amendment here. Hidden repository-only `test-clock` is not public,
is excluded from this subset enumeration, and never enters `full` or the
product default/maintainer release closures.
The checker reports hits/misses and atomically caches Cargo tree/metadata
outputs in `${CARGO_TARGET_DIR:-$ROOT/target}/feature-closure-cache.json` by a
SHA-256 of `cargo --version`, Cargo.lock, tracked Cargo.toml files,
`.cargo/config*`, rust-toolchain.toml, and the checker/policy sources (also
checkout identity because metadata embeds absolute paths); validation still
runs on hits, missing/corrupt caches recompute, and `--build`/`--no-cache`
bypass the cache. When `git ls-files` fails (including a container source
export without `.git`), the checker reports a cache bypass and queries Cargo
directly; no cache is read or written.

Closures prove dependency edges, not that the code compiles. Without
`--build`, the checker therefore also runs `cargo check --lib` on `plakous`
with no features and with each public feature alone (eleven profiles); a
rustc warning or error in any workspace crate of that profile fails it. Cargo's
fingerprints are its cache: an unchanged profile is a no-op (about 1 s for
all eleven), and a facade or dependency source change rechecks only the
affected crates. `--build` checks every subset and skips this step.

The checker also pins, over the complete closure Cargo reports, what a named
selection must *not* reach. A `composition` selection reaches no
`plakous-auth`, `plakous-http`, `plakous-py-eval`, `monty-pool`, `rquickjs`, or
`tungstenite`. That is what makes the decoupling provable rather than asserted.

`cargo tree` proves the closure, not that the crate compiles under those gates.
So `check_public_compile.py` also type-checks the two documented consumer
manifests on their own: `cargo check -p plakous --no-default-features
--features composition` and the same for `composition-python`, both locked and
offline. They are `check` rather than `build` because the question is whether
the feature gates leave a compiling crate. Every other positive in that checker
selects a session backend alongside the evaluator or the whole product closure,
so without these two neither documented manifest would ever be compiled.

`tools/checks/check_workspace.py` checks the same split at the artifact level,
not only in the manifests. It lists the symbols of the `plak`,
`plakous-js-host`, and `monty` executables and requires that only the
JavaScript worker contains QuickJS engine symbols, only the Python worker
contains Monty interpreter or type-checking symbols, and that the three are
separate files by digest.

Two details make that check sound. Plakous's own `RquickjsCodeModeIntegration`
type and `rquickjs_sidecar_v1` constructor carry "rquickjs" in their names and
are the sidecar client, so the check matches the engine's v0-mangled crate
names and C entry points rather than a substring. And it reads only
`target/release`, the product build: gate runs build `target/debug` with
`--all-features`, a different feature selection. Without a release build the
checker says the linkage was not checked rather than passing quietly.

Two further rules bind the check to the tree it is evidence for. A release
executable older than `Cargo.lock` or than the newest tracked Rust source is
refused as a stale release build and reported as not checked, so a binary from
an earlier commit cannot satisfy the current one. And the note the checker
prints carries the sha256 of all three executables, so a build receipt names
the exact artifacts that run read. The marker set covers both supported
platforms: the Rust v0-mangled crate names are platform-independent, and the
engine's C entry points are listed in both the Mach-O underscored and the ELF
bare spelling.

The
presentation stack (`ratatui`, `crossterm`, `pulldown-cmark`, `syntect`) is an
unconditional dependency of `bin/plak`. Its `tui` module is private to the
product library. There is no separate presentation package or `ui` feature.
Clipboard image paste (`Ctrl+V`) uses `arboard` (image data only: AppKit on
macOS, the pure-Rust X11 client `x11rb` on Linux, no Wayland feature), an
unconditional `bin/plak` dependency; on macOS `bin/plak` also depends on
`objc2`, `objc2-app-kit` (`NSPasteboard` only) and `objc2-foundation` to read
a screenshot's PNG flavour without the TIFF decode. Attached images are normalized in
`plakous-hosted` with the pure-Rust `image` crate (features `png`, `jpeg`,
`gif`, `webp` only). Both build for both supported targets, musl static
included.
`ratatui` does not appear anywhere in the `plakous-hosted` closure. The hosted
crate depends on the tree store port through `plakous-recursive-subagents`;
it has no dependency on the SQLite tree-store implementation package.

Hosted child startup uses the public `TurnCommand::Start` submission to retain
its original typed rejection acknowledgement. Recovery learns what became of a
Start from the child session's feed (`plakous::sessions::feed`); neither raw
store rows nor private runtime authority crosses that boundary. The checker names these public
symbols and permits JSON serialization only in the result-context module.
Host and product job construction use the facade's root `JobConfig`; product
job observations use root `JobStatus`, `JobStopReason`, and `JobView`; product
compaction telemetry uses `agent::CompactionRefusalReason`. These are enrolled in
the checker's approved-symbol inventory, without exposing private runtime authority.

The product's `carry/history.rs` alone may name the public model content
constructors and canonical-history validator needed to decode the neutral
export document (chapter 02 §2.5.1). Its closed tag-mapping macro and the
installed-build carry-capacity process probe are enrolled source-specifically;
this grants no mutable runtime/store authority or general source-generation
exception to the CLI. The facade inventory also pins the compaction replay
binding accessors and advisory `SessionRuntimeBinding::input_window`.
Hosted usage also names the facade's existing `openrouter::OpenRouterKeyUsageError`
to distinguish HTTP 401 and 403 and attach product-supplied safe key-source labels
and remedies; the error is enrolled in the same inventory.

API and gateway agent construction pair an explicit clock with the supported
`model::SystemIdGenerator`. The provider facade expiry constants expose
read-only compiled review metadata, never credential or runtime authority.
Both are pinned by the external facade inventory fixture and permitted by the
checker for their host/product consumers.
The facade reexports `openrouter::OPENROUTER_CHAT_PROTOCOL_ID`,
`openrouter::OPENROUTER_RESPONSES_PROTOCOL_ID`, and
`openrouter::OPENROUTER_MESSAGES_PROTOCOL_ID`; hosted admission uses these
provider-owned protocol names, enrolled in the same inventory.
The external facade consumer pins all four `JobStatus` variants (`Running`,
`Exited`, `Stopped`, `OutcomeUnknown`) and all five `JobStopReason` variants.

The repository rendering benchmark runs with
`cargo bench -p plak --bench tui_render --features test-support`. Its body
lives inside the private TUI module and is reached through the single hidden
`benchmark_tui_render()` entry point, compiled only with `test-support`.
Layout and test helpers do not become a public TUI API for the benchmark.

### 4.1 The `maintainer` feature

`maintainer` gates these verbs:

```text
fork   compact   compatibility
session rehearse <full-session-uuid>  # hidden, offline, read-only
```

`models` is not gated — it is an ordinary product verb alongside `login`,
`logout`, `status`, `refresh`, `key`, `run`, `agent`, `resume`, `session`,
`usage`, `doctor`, `config`, `telemetry`, `help`, and `version`.

Without the feature the maintainer tokens do not parse: argv carrying one is
rejected, they are absent from help, and the processing-tier selection is
inert. `tools/build/build-plak.sh` builds with `--features plak/maintainer`, so the daily
maintainer binary has them; an ordinary `cargo build --release` does not.

`session rehearse` inspects the current `PLAK_HOME` without creating product
state or opening credentialed providers. It reports the build ID, state key,
importable export versions and config verdict using the reload probe's policy
loader at the stored root workspace and each selected child cwd. Closed nodes
and unbound nodes report `skipped:<status>` without inspecting their worktrees
or sessions. Bound recovery-required nodes remain eligible for inspection.
Each exact root/selected-child head is decoded through the repository-only facade inspector (chapter 01
§12.1), which also reports the evaluator and process-budget verdicts.
For each exact root and selected-child head it reports the one recovery
verdict an open would execute (chapter 01 §5.5) and the number of feed records
the product and tree projectors are behind (`projects:<n>`, passing). It
applies those records in memory and validates the resulting views and the
root's TUI restore `PresentationSnapshot`. The product side never refuses; only
a head the verdict refuses fails the rehearsal. Inspection leaves the
databases byte-identical.
For children it validates a snapshot projected from only the first Forward
committed-history page (32 items, 4096 bytes). This is
neither a full transcript snapshot nor live attachment/recovery.
The report contains identities, counts and categories, never transcript,
checkpoint, credentials, arbitrary failure text or stored paths. Any offline
refusal exits nonzero. A passing report always states `not rehearsed: needs
credentials`: provider construction, harness compilation and runtime
revalidation are not exercised. Each session separately labels the stored
tool descriptor/presentation comparison as not rehearsed because it requires
credentialed runtime compilation. Success is an offline preflight, not a
promise that authenticated resume succeeds.

## 5. Unsafe surface

The workspace lint table sets `unsafe_code = "forbid"`, together with
`missing_docs = "deny"`, `rust_2018_idioms = "deny"`, `unused_lifetimes =
"deny"`, `unused_qualifications = "warn"`, and denied broken and private
intra-doc links. On the Clippy side, `all` and `pedantic` are warnings while
`unwrap_used`, `expect_used`, `dbg_macro`, `todo`, and `unimplemented` are
denied and `allow_attributes_without_reason` warns. Packages inherit this table with `[lints] workspace = true`, except for the
explicit local lint table described below.

`plakous-js-eval` maintains local documentation/Clippy defaults but still sets
`unsafe_code = "forbid"`; `plakous-js-runtime` inherits the workspace lints.
The sidecar client and evaluator use
safe Rust interfaces. Unsafe implementation inside upstream binding crates does
not require an in-tree allowance. Every local production crate, including the
vendored Code Mode protocol snapshot, forbids unsafe. Any exception requires a specification
change and focused review before the workspace policy changes.

## 6. Building, installing, and updating

Install the pinned verification tools once, which the gates require:

```sh
just dev-tools install
```

`scripts/dev-tools.sh install` installs exactly cargo-nextest 0.9.100 and
cargo-deny 0.19.0 into the repository-local `.dev-tools/bin`, which the
gate puts first on `PATH`. The prefix sits outside `target/` so `cargo clean`
does not remove it; its install build artifacts go to `.dev-tools/build`.
Installation also prefetches all locked public compile-contract fixture
dependencies (chapter 05 §1.1), so subsequent contract gates can stay offline.
Every `scripts/dev-check.sh` tier and `just deny` require at least those versions before they run
anything; a newer tool already on `PATH` is accepted.

### 6.1 Cargo-native product build

Kept installed snapshots record `state-key` and `export-version` next to the
binary, obtained from its hidden version answers. Foreign resume uses the
export-version manifest to refuse incompatible carry without running export.
Absent, empty or unparseable export-version manifests leave the version
unknown and retain the export-then-refuse fallback.

From the repository root, the newcomer path is:

```sh
just install
```

`just` is then a prerequisite (the getting-started guide states it).
`just install` runs `tools/build/build-plak.sh --install`: exactly the two
plain (non-`maintainer`) release Cargo invocations below, the artifact linkage
check, and the snapshot install described after the maintainer recipes, under
the release snapshot root `~/.local/lib/plak/release/<id>/` instead of the
maintainer root. Without `just`, the equivalent build is:

```sh
cargo build --release --locked -p plak
cargo build --release --locked -p plakous-js-runtime -p plakous-py-worker
```

The workspace default member is only `plak`. The parent and evaluator workers
MUST be built in separate Cargo invocations: Cargo's stable resolver unifies
`monty-proto/worker` features when the Python worker is selected alongside the
parent, linking interpreter/type-checking code into `plak`. Per-package feature
unification remains an unstable Cargo option on the pinned toolchain. The two
commands produce `plak`, `plakous-js-host`, and `monty` in the same directory.
Apply the same profile, target selection and target-directory settings to both
invocations; apply product features only to the parent. Bare `cargo build`
produces only the parent, not a complete install. The locked dependency graph
is Cargo-owned; no build script invokes Cargo or downloads dependency source.
Native dependencies still require a platform C/C++ toolchain and linker.
Development lanes of concurrent worktrees must use distinct Cargo target
directories: one shared target directory is safe only for one worktree at a
time. Share dependency compilation through sccache, not workspace artifacts.
Cargo defaults to the host target; the supported Linux musl build requires
`--target x86_64-unknown-linux-musl` and a musl C compiler/linker as described
in the getting-started guide. A GNU host build is not musl qualification.

For per-install native musl evidence on a Linux x86_64 host:

```sh
rustup target add x86_64-unknown-linux-musl
just musl-native          # defaults to --platform
just musl-native --fast plak  # optional targeted iteration
```

`scripts/musl-native.sh`, through `scripts/musl-toolchain.sh` (shared with
release packaging, §6.6), requires the installed rustup musl target and an
executable musl C compiler/linker. It takes the compiler from
`CC_x86_64_unknown_linux_musl` (default `musl-gcc` on `PATH`) and the linker from
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER` (default `rust-lld`). Rustc adds
its toolchain's linker directory to the linker `PATH`, so the bare name works
without a host-specific path. This links static PIE, the same layout as the
release musl binary, while musl-gcc compiles the C dependencies. Nix musl-gcc's
specs inject a dynamic loader for `-static-pie`: the binary can link successfully
but fail before even listing tests, so that compiler wrapper is unsupported as
the native linker. Caller-supplied compiler/linker values always win; the lane
sets no `RUSTFLAGS` default. Record any linker or Rust flag override with the
evidence. The Alpine qualification retains its own toolchain defaults.
No host-specific paths are embedded. Missing host, target, compiler or linker
prerequisites fail before Cargo with a message naming what is missing.
It sets `CARGO_BUILD_TARGET=x86_64-unknown-linux-musl` and forwards the tier to
`dev-check.sh`; `--platform` runs formatting, cfg-specific workspace clippy,
default/maintainer feature builds and the parallel, serial and durability
offline nextest lanes. It is genuine
musl build-and-test evidence on a GNU host, not Alpine container qualification.
Apple Silicon installs run `just platform` natively. Platform-independent
checks run once on Linux in `just check` and `just full` (chapter 05 §1).

Before publication, run the full offline musl
qualification on a Linux x86_64 Docker host:

```sh
just musl-check          # full tier, including default/maintainer checks
just musl-check --fast plak  # optional targeted iteration
```

`just musl-check` is a maintainer-only gate. `scripts/musl-check.sh` uses the
preloaded image `plak-qual-base:07d6b926` (image ID `07d6b926351c`), which is
not published; without that image on a Linux x86_64 Docker host the gate cannot
run, and the platform tiers above remain the public evidence. The image holds Alpine 3.24.2,
Rust 1.99.0 with a musl host toolchain, clippy, rustfmt and C/C++ tools, Python 3.14,
curl, git, jj, just, and the registry sources for cargo-deny (not its executable),
the workspace and compile-contract fixture lockfiles. It never pulls an image or installs tools.
Provide the executable musl cargo-nextest 0.9.100 in `~/plak-musl/bin`
(override with `PLAK_MUSL_TOOLS`); GNU-host tools cannot substitute for it.
The container has no network and sets `CARGO_NET_OFFLINE=true`. A dependency
change requires refreshing the image's registry before qualification.
`scripts/musl-image.Dockerfile` is the recipe: built from a `git archive` of a
commit, its default target is the image, tagged `plak-qual-base:` plus the first
eight hex digits of its image ID, and its `nextest` target exports the musl
cargo-nextest at the version `scripts/dev-tools.sh` pins. A toolchain or
dependency change rebuilds the image under a new tag and updates the tag in
`scripts/musl-check.sh`.

The script exports tracked and nonignored untracked source, including the
working-tree diff and excluding deleted files, into a fresh retained
`target/musl-source.*` directory. Neither `.git` nor ignored host `.dev-tools`
or build artifacts enter the container. Its target cache defaults to
`target/musl-gate` (override with `PLAK_MUSL_TARGET_DIR`); musl tools are
mounted read-only, and test scratch uses an executable 16 GiB `/dev/shm`.
The container runs with the caller's UID/GID, so exported source and target
artifacts remain caller-owned.
Cargo artifacts use `/qual/target/artifacts` inside the writable target mount,
not under the root-owned `/qual` directory. Cargo-tree structure checks fail
closed and report the failed command, exit status and Cargo stderr, including
offline registry resolution errors.
A temporary writable `HOME` is separate from the persistent writable
`CARGO_HOME` under the target cache's `cargo-home-<tag>/`,
one directory per pinned image tag, so a bumped image never reuses an older
image's crates. The image's offline registry and git caches are copied only on
the first run for that image, with the seed marker written only after copying
succeeds; subsequent runs reuse them. `RUSTUP_HOME` uses the image's
preinstalled toolchain. No root-owned build or test files land in the checkout.
Build parallelism defaults to the host's online core count, or the caller's
`CARGO_BUILD_JOBS`.
`CARGO_BUILD_TARGET=x86_64-unknown-linux-musl` selects the explicit `--target`
arguments in the gate and the inherited target for helper Cargo invocations.
Output streams to the caller and ends with `EXIT <status>`; the script returns
that same status. Record the source revision/diff and both nextest lane counts
with the result; the image name alone is not evidence for the current tree.

`plakous-js-eval` contains only the protocol and sidecar client.
`plakous-js-runtime` owns QuickJS and the JavaScript worker. Selecting that
package alongside Plak cannot enable an in-process runtime feature in Plak.
The Monty worker is a thin Rust process entry point over `monty-proto`'s
upstream `Child`, framing, interpreter and type checker, pinned at `1.2.0`
together with `monty-pool`, `monty-types`, and `monty-alloc`. Its shell follows
upstream `monty-runtime` 1.2.0: a 16 MiB worker thread stack, the allocator's
hard ceiling at the interpreter's soft memory limit plus upstream headroom,
fatal frames, panic handling, EOF and shutdown behavior. Only `monty subprocess` and `monty --version` are accepted.

For maintainer installation:

Before asking an owner to `/reload`, run from the candidate checkout:

```sh
just reload-rehearsal <full-session-uuid> [--installed <path>] [--config-overlay <dir>]
PLAK_HOME=/absolute/home just reload-rehearsal <full-session-uuid> [--installed <path>] [--config-overlay <dir>]
# From a live turn, detach the wait and optionally install after a passing run:
just reload-rehearsal <full-session-uuid> --wait-idle --timeout 300 --detach --log <new-log-path> --install
```

`scripts/reload-rehearsal.sh` builds the maintainer binary and both workers
offline into Cargo output using the same separate parent/worker invocations and
`plak/maintainer` feature selection as `tools/build/build-plak.sh --maintainer`,
then copies them into a disposable staging directory,
and by default never invokes the installer or changes installed links. SQLite's backup
API reads committed WAL state through read-only source connections into a
private temporary `PLAK_HOME`. The staged version/build ID is printed even if backup is refused; for the same
source, toolchain, target, build environment and same checkout path it matches what
`just build-maintainer` installs (`build-plak.sh` refuses linked worktrees).
The requested session's source key is located
by read-only sessions-table ID membership. When it matches the staged key, only
that key's runtime, product and tree are backed up and `plak session rehearse`
runs there. When it differs, the report says `state key changed <old> -> <new>:
rehearsing carry-forward` and the tool rehearses carry-forward in disposable
homes: the installed `plak` on `PATH` (or `--installed <path>`) exports the
session from a copy of the source key (`plak session export <id> --out <tmp>`);
export runs from the session record's `workspace_root`, read from the copied
product database through a read-only connection, not from the build checkout.
A missing record or missing/non-absolute workspace refuses with
`export refused: session_workspace_unavailable`; there is no checkout fallback.
A second home receives the non-secret allowlist and no state for the new key;
`--config-overlay <dir>` replaces `config.toml`, `agents/` and `profiles/` with
that directory. The staged build then runs `plak config show` and
`version --reload-probe`. A config error is printed verbatim with filesystem
paths removed, and import does not run. Otherwise the staged build imports the
document and runs `plak session rehearse <new-id>`. The report prints the new
id, the carried-from line, and the root and child rehearse verdicts. Exit 0
requires export, config validation, import and rehearse to succeed, except an
import whose only refusal is `plak: session has no exact effort binding` (the
disposable home has no credentials, so the Ready catalog cannot admit the
binding): that reports `import=not rehearsed: credentials unavailable in the
disposable home (export, config and document conversion rehearsed)` and exits 0
without rehearse.
The allowlist copies
`config.toml`, `trusted-projects.toml`, `agents/`, `profiles/` and `references/`;
links and special files in these inputs are refused. Secrets, subscription/XDG
state, holder locks and event logs are never copied. Credential, provider,
telemetry and test-hook environment overrides are not inherited by the installed
or staged command. Historical recorded evaluators are checked at their original physical
paths, not replaced by the staged workers. The temp homes and staged executables
are removed on success, failure, SIGHUP, SIGINT or SIGTERM. No live session or workspace is mutated.
Backup operations have a deadline; the inspector has a process timeout.
Databases are separate snapshots: concurrent publication can produce an exact
head refusal; retry while the TUI is idle rather than ignoring it.
`--wait-idle` retries only the exact installed-build export refusals for a
running or parked turn, a runtime ahead of its session record, or unsettled
background-process outcomes.
It takes new read-only backups on every attempt, with a fixed 15-second delay
between attempts to avoid thrashing large stores, until export
succeeds or the idle-wait deadline expires (`export refused: wait_idle_timeout`).
`--timeout S` is a positive finite number of seconds, defaults to 300, and
requires `--wait-idle`; build/staging time is outside this idle-wait deadline.
`--interval S` overrides the delay, is positive and finite, and also requires
`--wait-idle`. Detached runs retain the selected timeout and interval.
Every other export refusal fails immediately, without import or installation.
`--detach --log <path>` requires `--wait-idle`, starts a new POSIX session
(`setsid`) with no input terminal, and redirects output to a newly created
mode-0600 log outside the live home. Existing logs/links are refused rather than
overwritten. The caller receives the child PID and may end its turn; the
detached run retains the build checkout and absolute installed/overlay paths.
`--install` runs `just build-maintainer` from that checkout only after a passing
rehearsal, after disposable copies are removed; the installer's linked-worktree
guard still applies. It never interrupts a running session; `/reload` remains
the operator's action. The existing credential-unavailable import exception is still
a passing, explicitly partial rehearsal, not proof of an authenticated import.
A live-session race — a database busy or locked, the source changing during
capture, or a carry already in progress — prints only
`reload rehearsal refused: session_busy_retry_when_idle`. Any other staging or
backup exception prints only
`reload rehearsal refused: staging_or_backup_unavailable`. Neither line
includes paths, SQL, or config.

Then, after the offline gate and ordinary checks:

```sh
just build             # debug build + local symlinks
just build-release     # release build + local symlinks
just build-maintainer  # dogfood build: release codegen, no LTO, incremental
```

These add `--features plak/maintainer` and copy `plak` into an immutable snapshot directory
`~/.local/lib/plak/maintainer/<id>/` (`just install`: the same install without the
feature, under `~/.local/lib/plak/release/<id>/`), where `<id>` is the SHA-256 of a
manifest of all three executables' SHA-256 digests. Each worker is installed
independently at `~/.local/lib/plak/sidecars/<name>/<sha256>/<name>`; the
snapshot contains symlinks to those immutable mode-0555 artifacts. Their
canonical paths depend on their own bytes, not the parent Plak build, so an
unchanged evaluator remains identical across parent-only rebuilds. Cached
artifacts with changed bytes or permissions, or symlinked cache directories,
are refused rather than overwritten. Workers and cache directories must belong
to the installing user; directories must not be group- or world-writable.
New owned installation ancestors, cache roots and engine directories use mode
0755 regardless of the user's umask. Existing physical owned ancestors are
checked for ownership and group/world writability, including through linked
homes; unsafe entries are refused, never silently repaired. Root-owned system
ancestors must be non-writable or sticky temporary roots; other owners are refused.
Concurrent identical installs retain one
artifact. The installer canonicalizes
the existing snapshot root with `pwd -P` before composing snapshot paths, so
linked home ancestors preserve canonical executable paths. A
rebuild that changes only a sidecar is a new snapshot (staged then renamed into
place; a concurrent build of the same bytes keeps one copy). The
`~/.local/bin/plak` symlink points at the snapshot, while
`~/.local/bin/plakous-js-host` and `~/.local/bin/monty` point at the worker artifacts;
corresponding existing paths under `~/.cargo/bin` are repaired. An existing
link is replaced only when it already resolves under the maintainer or release
snapshot root, the shared sidecar cache, or the Cargo target directory; any
other file or link is refused. A rebuild never
rewrites a file a running session executes, so a running TUI can offer
`/reload` (chapter 02 §5.1). A running `plak` finds its sidecars beside the
snapshot file it executes, not beside the `PATH` symlink. Each snapshot also
holds a mode-0444 `binary-id` manifest (`id dev ino size mtime_ns`). After the
executable's final chmod and before renaming the staging directory, the
installer runs `plak version --write-binary-manifest <staging>` to hash and
stat the same open executable. Directory rename preserves those identity
fields; a matching manifest avoids hashing on startup (chapter 02 §1). This
manifest is trusted like the private installation directory, not as a signature.
It is generated at installation, not shipped in the release tarball. If the
probe fails (including a release without that flag), the manifest is absent
and installation continues; that binary hashes its executable as before.
Each snapshot also holds a `state-key` manifest, the output of the snapshot's
own hidden `plak version --state-key` (absent if that fails), which is how a later build
finds the build that can export an old key's sessions (chapter 02 §2.5.1).
The matching snapshot, rather than the new build, creates the bounded restart
checkpoint and readable archive; the new build never carries an unbounded set
of historical decoders or opens the old snapshot's state directly. If the
matching snapshot is unavailable, reference creation and incompatible reload
fail explicitly and leave the old state untouched.
That discovery searches both the maintainer and release snapshot roots and
only considers directories named by 64 lowercase hex digits whose directory and `plak` are owned by the current user and not group- or
world-writable. All snapshots, parent binaries, manifests and worker artifacts
are retained until explicit operator cleanup, including legacy snapshots with
physical workers and snapshots without manifests. There is no automatic pruning
or process-probe retention policy: a stale listing must not delete a snapshot
another installer has just reused, and historical producers remain available
for carry and recovery. Abandoned staging directories likewise require explicit
cleanup; an installer removes only its own duplicate staging copy. Cleanup must
preserve binaries and canonical worker paths needed by running or historical
sessions. Use a release
or maintainer build for daily work because debug binaries are large and slow
to start.

At run time, js-ptc and Codex native-ptc locate `PLAK_JS_HOST` or a sibling
`plakous-js-host`, and py-ptc locates `PLAK_MONTY_WORKER` or a sibling `monty`.
Keep all three executables together when installing manually. Missing workers
fail closed and `plak doctor` names the missing configuration. Official V8
and `@openai/codex` are not fetched and are not evaluators.

The maintainer installer and reload-rehearsal staging require Python 3.11+ and
`tools/build/check-artifact-linkage.py` over the produced artifacts before use.
It matches QuickJS entry points and length-prefixed Monty runtime crate names
in raw ELF or Mach-O symbols, so Apple's `nm` needs no demangling support.
Both worker positive controls and distinct artifact digests are mandatory;
missing artifacts, a missing symbol lister or an unreadable listing fail closed.
For release-profile helper builds, Cargo outputs retain raw symbols solely for
this check (`CARGO_PROFILE_RELEASE_STRIP=debuginfo`). After successful proof,
the helper copies all three executables into the profile's `stripped/`
directory and strips those copies. Release installs and `--artifacts` use
only these staged copies; the original Cargo outputs remain inspectable.
The gate's `builds` group reuses its default worker builds and isolated
maintainer parent build to require the same check on both supported platforms;
it runs in `check`, `platform` and publication `full`, without another release
build. Workspace all-features test binaries are not shipped artifact evidence.

### Embeddable library build

Embedding an agent and embedding an interpreter are separate decisions. The
admitted model, the evaluator deployment, and the durability mode are chosen
independently, and an unsupported combination is refused rather than repaired.

For Code Mode and JavaScript through the worker:

```sh
cargo build --locked --release -p plakous --no-default-features --features composition
```

Both evaluators run in worker processes; Plakous has no in-process evaluator
option.

#### Which patch tables a consumer needs

Cargo does not inherit `[patch]` tables from a path dependency, so a downstream
source consumer repeats in its own root manifest whatever its selection needs.
No facade selection needs one, including those that reach the Python
evaluator. Only building the Monty worker package itself selects the reviewed
`monty-typeshed` snapshot (§7.3), and a consumer that omits it builds the
upstream crate, whose type-checker archive is not byte-reproducible.

There is no Codex patch table. `plakous-code-mode` depends on the
reviewed vendor snapshot `vendor/codex-code-mode-protocol`, the package
`plakous-codex-code-mode-protocol` (the upstream crate renamed so it can be
published without claiming the upstream name), by path and version; its
`PROVENANCE.md` records the upstream repository, revision
`b1a547b1f73ce86205d9222ac19cff334b3b7a2e`, version `0.153.3`, Apache-2.0
license, and the exact files and line ranges copied.

#### Runnable external examples

Two fixtures are their own Cargo workspaces, import only the supported facade,
and need no `test-support` feature or crate:

```sh
cargo run --manifest-path tests/fixtures/js-embedder-consumer/Cargo.toml --locked --offline
cargo run --manifest-path tests/fixtures/facade-consumer/Cargo.toml --locked --offline
```

`facade-consumer` is the broad public inventory contract and selects every
feature. `js-embedder-consumer` is the JavaScript-only workflow. Neither carries
a `[patch]` section; `check_public_compile.py` fails if one appears in the
workflow fixture.

Five copyable examples cover one workflow each. Each names its exact features,
and none depends on a repository test-support crate:

| Example | Features | Workflow |
| --- | --- | --- |
| `cancellation_shutdown` | none | Cancelling one turn, then terminal shutdown |
| `direct_custom_tool` | `sessions-sqlite` | A caller-owned tool exposed directly to a Custom model, streamed turn events, and reopening from a persisted locator string |
| `durable_session` | `sessions-sqlite` | Create, release, and reopen by locator |
| `provider_direct` | `openrouter` | A built-in provider on Direct, with the API key read from a file path |
| `worker_js_sidecar` | `composition`, `sessions-sqlite` | A caller-owned nested tool called from JavaScript in `plakous-js-host` |

```sh
cargo run -p plakous --example durable_session --features sessions-sqlite
```

`provider_direct` takes a key-file **path**, never the key text, and with no
arguments it says what it needs and exits: it discovers no credential.
`worker_js_sidecar` takes the built worker path as an explicit argument, so the
offline gate compiles it and the operator runs it against a built worker. The
other four run to completion offline in the gate. The standard coding packs
contain mutating tools, which require a required-durability session store, so
the composition example also selects a session backend.

`crates/plakous/tests/agent_scripted_quickstart.rs` is a repository offline test that drives
`plakous-test-support`; it is not an external example and chapter 05 owns it.

Retain a downstream lockfile and fetch its dependency closure before using
`--locked --offline`. Each fixture's checked-in manifest and lockfile pin the
complete example, including utility dependencies. No registry publication or
independent binary distribution is established by these examples.

The root `[patch.crates-io]` selects the reviewed `vendor/monty-typeshed`
1.2.0 dependency snapshot. Only its build script and ZIP dependency change:
ZIP timestamps are fixed to 1980-01-01, entries are sorted by name, and the
archive is written and read with `zip` 9 instead of the advisory-affected
`zip` 0.6. The Python stubs and interpreter remain unchanged. The snapshot's
provenance is recorded in `vendor/monty-typeshed/PLAK-PATCH.md`; it is
excluded from workspace membership
and compiled by Cargo as a dependency. This eliminates per-build archive
extraction and patch application while preserving deterministic type-checker
assets. The shared lock
pins `salsa`, `salsa-macros` and `salsa-macro-rules` at 0.28.2 and the Ruff/ty
crates at 0.0.14, the versions upstream Monty 1.2.0 locks: `salsa` 0.28.5 does
not compile with `ruff_python_ast` 0.0.14. `just deny` audits the unified
workspace and worker dependency graph.

Three profiles beyond the Cargo defaults exist. `release` uses
`codegen-units = 1`, speed-focused `opt-level = 3`, fat LTO, and
`strip = "symbols"`. Panic unwinding remains enabled: stripping retains unwind
tables and panic messages (including source locations), but release backtraces
lose symbol names and source-line resolution. Use the maintainer or profiling
profile for symbolized backtraces. `maintainer` inherits
release without LTO, with 16 codegen units, incremental compilation and line
tables, so dogfood rebuilds are fast and backtraces name source lines;
`just build-maintainer` builds it. `profiling` inherits
release with full debug symbols, no stripping, and LTO off so frames stay
attributable; `just build-profiling` produces `target/profiling/plak`, and
`just samply`, `just flamegraph`, `just perf-stat`, and `just bloat` drive it.

#### Release size and speed qualification

Linux x86_64 GNU measurements qualify the release profile's size/build-time
tradeoff; these are non-publication rehearsals, not musl or macOS evidence.
Both columns use Git-only defaults, unchanged AWS-LC-backed JWT verification,
and separate parent/worker Cargo resolutions. Sizes are bytes; raw means the
symbol-bearing Cargo output used for strict linkage proof. Staged copies have
all symbols stripped. The tarball includes all three executables and licenses.

| Artifact | Thin raw | Thin stripped | Fat raw | Fat stripped |
| --- | ---: | ---: | ---: | ---: |
| `plak` | 81,903,112 | 67,232,472 | 71,031,984 | 61,277,272 |
| `plakous-js-host` | 2,500,000 | 2,080,016 | 2,298,784 | 1,988,720 |
| `monty` | 35,642,912 | 28,802,632 | 32,455,792 | 27,440,648 |
| GNU rehearsal `.tar.gz` | - | 41,632,982 | - | 40,392,040 |

| Wall time | Thin | Fat |
| --- | ---: | ---: |
| Parent Cargo build | 7m 14s | 8m 47s |
| Worker Cargo build | 5m 55s | 6m 34s |
| Complete `--artifacts` build, linkage proof and staging | 812s | 928s |
| Tarball packing | 12s | 10s |
| Complete build and packing | 824s | 938s |

Fat LTO removes 5,955,200 bytes from stripped Plak for 13.8% more total
release-workflow wall time. These are sequential shared-host measurements with
warm dependency caches, not a clean-build benchmark. The maintainer profile
continues to override LTO off, so this tradeoff does not change its codegen.

Warm process-launch medians (11 samples) are 52.47ms for the original
symbol-bearing default build, 42.31ms after stripping that same executable,
37.09ms for Git-only thin LTO, and 34.64ms for Git-only fat LTO. The original
build is 90,227,472 bytes raw / 73,734,584 stripped: stripping and omitting JJ
account for the reductions before LTO. No global or per-dependency size-oriented
optimization level is selected.

The deterministic offline `test-hooks` fixture, measured separately with seven
runs per variant, compares the original default build with Git-only, stripped
thin LTO. Offline `run` medians are 54.84ms / 33.88ms and `session show` medians
are 7.80ms / 5.03ms. Their `--version` medians (11 samples) are 59.38ms / 40.43ms.
These instrumented sizes and timings are not shipped-artifact measurements;
fat-LTO offline-turn and session-show timings are not established by this table.

### 6.2 Isolated playground

```sh
just build-release && just plak-dev-install
```

`plak-dev-install` points `~/.local/bin/plak-dev` at `scripts/plak-dev.sh`,
repairing a stale symlink from an earlier layout, and seeds
`~/plak-dev-home/config.toml`. The wrapper pins the login user's `HOME`, uses
the isolated `~/plak-dev-home`, and unsets provider API keys so they cannot
displace OAuth. Run `plak-dev` from a scratch working directory, never from the
git tree, and never point `PLAK_HOME` at the worktree. `just plak-dev-reset`
wipes the `state/`, `telemetry/`, `logs/`, and `secrets/` roots in that home
while keeping `config.toml` and `agents/`.

The launcher defaults to `target/release/plak` (`PLAK_DEV_PROFILE=debug`
selects debug, `maintainer` the `just build-maintainer` output) and clears `PLAK_TEST_EXCHANGE_CAPTURE_DIR`, so the binary
uses its own passive capture. It retains argv and cwd and uses `exec` so
signals reach the binary directly. A `maintainer` binary captures inference
passively into a fresh private run/process directory under
`$PLAK_HOME/wire-captures/` (mode 0700; an insecure root disables capture),
including normal installed CLI, headless and TUI use. The run is selected on
the first inference transport, not at process startup; commands that send
no inference create no capture directory. An explicit
`PLAK_TEST_EXCHANGE_CAPTURE_DIR`, available with `maintainer` or `test-hooks`,
is the fail-closed qualification hook: it must name a private run directory
under the checkout's ignored `tests/provider-authority/`, and a capture that
cannot be written fails the exchange. Builds without either feature refuse
an explicitly requested capture rather than silently ignoring it.
Chapter 05 (§2.5, §3.5) describes the bounded, required capture and its
privacy and coverage limits.

### 6.3 Updating

Rerun the release installer (§6.6), or pull the source and rebuild:

```sh
git pull && just install
```

(Maintainers use `just build-release` or `just build-maintainer`.)

Either installs a new snapshot and moves the symlinks from §6.1 to it, so
new launches use the new binary once the install succeeds; a running TUI shows
`new build installed · /reload`. There is no in-product updater.

### 6.4 Linkers

The repository ships no Cargo linker configuration: `.cargo/config.toml` sets only
the resolver, and ordinary product builds link with the toolchain default.
The native musl qualification script selects the environment's compiler/linker
and defaults to rust-lld for static PIE as described in §6.1. A
contributor who wants faster Linux links configures `clang` plus `mold` for
`x86_64-unknown-linux-gnu` in their own Cargo configuration, outside the tree.

### 6.5 Publishing the cohort

`scripts/publish.sh` (`just publish`) is the only publication path. It stages
the tracked tree with the published `plak` manifest (§3.3), derives the order
from `cargo metadata` there (`tools/release/publish_plan.py`: every member
whose `publish` is not `false`, dependencies first, where normal, build and
versioned dev-dependencies order the cohort because crates.io resolves each at
upload; a cycle is an error), and runs one `cargo package --locked` over that
selection. Cargo packages the members against a local overlay of the archives
it has just written, so unpublished siblings resolve, and verifies each by
building it from its archive, with `CARGO_TARGET_DIR` at
`<target>/package`. The vendor snapshot is a workspace member (§3) precisely so
the overlay includes it. Cargo unpacks overlay archives into the Cargo home
under their name and version and reuses an existing unpacked copy, and it
treats a registry package's build in the verification target as fresh by
package identity alone, so before packaging the script removes every unpacked
`<name>-<version>` of the cohort and the cohort's verification fingerprints;
otherwise sources or artifacts left by an earlier run of the same version
would be verified in place of the tree being packaged.

A packaged crate resolves its registry dependencies without the root
`[patch]` table, so verifying `plakous-py-worker` needs the upstream
`monty-typeshed` (§6.1), which `Cargo.lock` does not name and `cargo fetch`
does not download. The `package` group therefore prepares the archives with
`cargo package --locked --no-verify`, offline first and online only if cache
inputs are missing. It extracts precisely those archives into disposable
scratch directories and fetches each archive's dependency graph with the same
offline-first policy. Only the cohort siblings are substituted with their
extracted archives during fetch, so unpublished siblings resolve without
restoring the workspace's third-party patches. Fetch may update the disposable
locks for those local substitutions; verification uses the original archives
and their locks. The final `cargo package --locked` builds and verifies strictly
offline. An explicit `CARGO_NET_OFFLINE=true` remains authoritative during
prefetch as well. No separate warm-up is needed. The verification target directory holds
a debug build per crate selection and is roughly 20 GB.

| Invocation | Preconditions | Effect |
| --- | --- | --- |
| `scripts/publish.sh` | More than 21 days on every compiled review; clean git tree; `<target>/gate-evidence/full` names `HEAD^{tree}` | Package and verify; nothing uploaded |
| `scripts/publish.sh --gate` | None (review time is informational) | Offline-first verification-input prefetch, then strictly offline packaging and verification |
| `scripts/publish.sh --execute` | The dry run's, plus `CARGO_REGISTRY_TOKEN` | After packaging, `cargo publish --locked --no-verify -p NAME` per crate in order; before each, a crate already in the index at its version is skipped, and after each the script polls the sparse index (`PLAK_PUBLISH_INDEX`, default `https://index.crates.io`) until the version appears, failing after `PLAK_PUBLISH_TIMEOUT` seconds (default 600). A rerun resumes |

Publication dry runs and `--execute` use `tools/release/review_expiry.py` to
read all seven compiled provider expiries and refuse any with 21 days or less
remaining, failing closed on unreadable source. The `--gate` package group
prints remaining days as a non-failing informational note instead. Neither
`just check` nor `just full` has a time-dependent review-window precondition.
The hidden `test-clock` feature is excluded from supported feature closures
and inventory, and checked absent from default, maintainer and release builds;
only dev-dependencies or repository `test-hooks` enable it.

On a colocated JJ root, `just full` records the Git tree of snapshotted `@`
at start and end, refuses conflicted `@`, and requires equal trees. `@` need
not be empty. Evidence is recorded only where `.git` is a directory, never
in linked Git or secondary JJ workspaces; their package dry run still uses
the shared VCS listing. Copies reuse the launch's `.dev-tools` when they have
none of their own. Install/relink builds likewise require a `.git` directory.
The evidence file is written only by a passing `just full` that began and
ended on the same clean Git tree or unconflicted colocated JJ tree (chapter 05 §1),
so an upload always follows a full gate on exactly the bytes it packages. Publishing is a maintainer act;
no gate or workflow uploads. The maintainer runs `just full` and then
`just publish --execute` on a gated host, from the tagged tree whose release
tarballs §6.6 builds.

For colocated JJ: test `@` with `just full`, then `jj commit` or `jj new` so
`@` is empty before `just publish` (which still checks clean Git status and
`HEAD^{tree}`). Tag `@-` with `jj tag set vX.Y.Z -r @-` or Git, set the main
bookmark to `@-`, push it with JJ and push tags with Git. Never push
`refs/plak/*` (in particular, never `git push --mirror`). Release automation
continues to clone plain Git.

### 6.6 Release artifacts and the installer

`tools/release/package-release.sh [--untagged | --tag TAG] [--out DIR]` builds a release
on the host's supported target: `x86_64-unknown-linux-musl` on Linux x86_64,
with the compiler, linker and prerequisite checks of `just musl-native`
(`scripts/musl-toolchain.sh`), and `aarch64-apple-darwin` on Apple Silicon. It
refuses a dirty tree and, unless `--untagged` (a rehearsal), a `HEAD` without
the tag `v<version>` of the workspace version, or without the tag `--tag`
names. `--tag` accepts `v<version>` or a release candidate `v<version>-rc.<n>`
(`n` from 1); a candidate's tarball is named `plak-<version>-rc.<n>-<triple>`
while its executables still report `<version>`, so the final tag can name the
same qualified commit. The build is
`tools/build/build-plak.sh --artifacts`: the `just install` Cargo invocations
and artifact linkage check, without installing. It maps the Cargo home, the
checkout and the target directory to `/cargo`, `/plak` and `/target` with
`--remap-path-prefix` in `CARGO_ENCODED_RUSTFLAGS` and `-ffile-prefix-map` in
the target's `CFLAGS_<triple>`/`CXXFLAGS_<triple>`, so neither Rust panic
locations nor vendored C assertions carry build-host paths; caller rustc flags
go in that variable too, and `RUSTFLAGS` is refused because Cargo would ignore
it. `tools/release/release_tarball.py` then writes
`plak-<version>-<triple>.tar.gz` and rewrites `SHA256SUMS` over every release
tarball in the output directory (default `<target dir>/release-dist`). The
tarball holds one directory `plak-<version>-<triple>/` with `plak`,
`plakous-js-host`, `monty`, the repository `LICENSE`, `THIRD-PARTY` (each
package outside this repository, vendored ones included, that the three
executables reach through normal dependency edges for the target, with its
`cargo metadata` license and repository) and `licenses/<name>-<version>/`
holding the license, copying and NOTICE files those packages ship. Entries are
sorted, owned by 0:0 and stamped with the release commit's time, so the archive
bytes depend only on the packed files. The Linux executables must be
static: an ELF program interpreter fails packaging.

`.github/workflows/release.yml` is the repository's only workflow. A pushed
`v*` tag runs `package-release.sh --tag <tag>` on a GitHub macOS arm64 runner
and an Ubuntu x86_64 runner with `musl-tools`, attests each tarball with
`actions/attest-build-provenance`, and creates the GitHub Release with both
tarballs and their combined `SHA256SUMS`, marked as a prerelease for a
`-rc.<n>` tag so `releases/latest` never resolves to a candidate. It never publishes crates: a runner
has no full-gate evidence for the tree (§6.5). Every action is pinned to a full
commit SHA.

`install.sh` at the repository root is a POSIX `sh` installer that never
needs elevated privileges:

```sh
curl -fsSL https://raw.githubusercontent.com/N0xMare/plak/main/install.sh | sh
PLAK_VERSION=0.0.1 sh install.sh   # a specific release instead of the latest
```

It supports exactly `aarch64-apple-darwin` (including an x86_64 shell under
Rosetta) and `x86_64-unknown-linux-musl`; any other host is refused with a
pointer to the source build. It resolves the latest release from the
`releases/latest` redirect, downloads the tarball and `SHA256SUMS` over HTTPS
only, and refuses a missing entry or checksum mismatch before writing
anything under `~/.local`. When `gh` is installed and signed in it also runs
`gh attestation verify --repo N0xMare/plak` and refuses on failure;
otherwise it reports that only the checksum was verified
(`PLAK_SKIP_ATTESTATION=1` skips the step). `PLAK_RELEASE_BASE` replaces
`https://github.com/N0xMare/plak` for mirrors and tests and lifts the HTTPS-only
restriction. The install is exactly the §6.1 layout: the snapshot
`~/.local/lib/plak/release/<id>/` with the same `<id>`, its install-time
`binary-id`, `state-key` and `export-version` manifests, the shared mode-0555 workers under
`~/.local/lib/plak/sidecars/`, the same ownership and writability refusals for
installation directories and their physical ancestors, and `~/.local/bin`
links that replace only links already pointing into the maintainer, release
or sidecar roots. A release install and a `just install` of the same bytes
therefore share one snapshot, and `plak doctor`, carry discovery and `/reload`
behave identically. `~/.cargo/bin` is not touched; the installer notes a `plak`
earlier on `PATH`. Downloads use `curl`, which sets no quarantine attribute on
macOS.

## 7. Dependency and supply-chain policy

A dependency is taken only when it provides a clear correctness,
interoperability, or maintenance benefit over a small local implementation. Each
material choice records its maintenance, correctness, MSRV, license, security,
and feature-cost reasoning. Default features are reviewed rather than accepted.

Native JJ uses crates.io `jj-lib =0.46.0` (Apache-2.0, upstream MSRV 1.97.1),
with default features disabled and only `git` enabled; Watchman is excluded.
Its Git backend uses `gix =0.88.0` in process; a matching direct dependency
with defaults disabled accesses immutable candidate refs in that existing Git
backend, without importing Git types into the workspace port. `pollster =1.0.1` drives the library's
async local operations without a nested Tokio runtime or detached worker.
The workspace's own unsafe ban is unchanged; as with other dependencies it
is not a blanket claim that transitive upstream crates contain no unsafe.
Repository types remain private to `plakous-workspace`. Compatibility with
installed external JJ 0.44 through 0.46 executables is covered by real
repository tests. The supported CLI range is the library minor and its two
previous minors; a newer CLI is unsupported until the library is bumped.
Each bump updates `JJ_LIB_VERSION`, `JJ_CLI_SUPPORTED`, the user guide and
reruns JJ tests against the lowest and highest supported binaries. The opt-in
`just jj-compat` lane (not a gate tier) tests all three pinned minors;
`just dev-tools install` installs checksummed release archives under
`.dev-tools/jj/<version>/jj`. Fixtures honor `PLAK_TEST_JJ`;
this is not a promise of compatibility with arbitrary custom backends or
future repository formats.

The MCP client (`plakous-mcp`) uses crates.io `rmcp =3.5.1` (Apache-2.0,
upstream MSRV 1.88, edition 2024), the reference Rust SDK of the protocol, so
protocol-revision negotiation, the `initialize` and no-initialize lifecycles,
cancellation and progress correlation are not re-implemented. Default features
(`base64`, `macros`, `server`) are off; the product graph enables only
`client` and `transport-async-rw`, which add no crate beyond `rmcp` itself to
the lock graph (`chrono`, `futures`, `indexmap`, `pin-project-lite`, `thiserror`,
`tokio-stream`, `tokio-util` and `tracing` were already present). The
streamable HTTP client runs over `plakous-http` through rmcp's
`transport-streamable-http-client` feature (`client-side-sse` and
`transport-worker`), never `transport-streamable-http-client-reqwest`: reqwest
would add a second TLS and proxy stack (it reads system proxy variables) to the
product graph for one client. That feature adds `sse-stream` 0.2 (MIT OR
Apache-2.0; `plakous-mcp` names it only for rmcp's event type and frames bytes
with `plakous_http::sse`), plus `base64` 0.23, `http` and `bytes`. `base64` 0.23
was already locked beside the workspace's 0.22 (through `hyper-util` and
`reqwest`); rmcp requires 0.23 and the workspace's own users stay on 0.22, so
the duplicate is accepted under `multiple-versions = "warn"`. Not taken:
`transport-child-process` (`process-wrap`; servers start through the execution
backend's service-process seam), `which-command`, the `reqwest` HTTP clients,
`auth`/`oauth2` (MCP OAuth is built on `plakous-auth` and `plakous-http`
instead, adding no crate to the lock graph), `macros` and `schemars`. The
resource and prompt requests are part of the `client` feature already
enabled. The repository-only scripted server
`plakous-mcp-fake-server` enables `server`, `transport-io` and
`transport-streamable-http-server` (serving through `hyper` directly), which
pull `schemars`, `pastey`, `dyn-clone`, `async-trait`, `rand`, `tower-service`
and `uuid` into the test graph only.

The Plak-run `fetch_url` (`plakous-tool-web`, feature `fetch`) converts HTML to
Markdown with crates.io `htmd` 0.5.6 (Apache-2.0, edition 2024, no stated MSRV;
builds on the pinned toolchain). Markdown keeps the headings, links, lists and
code blocks a model reads, which a text renderer such as `html2text` flattens.
It parses with `html5ever` 0.38 into `markup5ever_rcdom` 0.38.0+unofficial
(both MIT OR Apache-2.0, MSRV 1.71; the rcdom release is the one htmd
publishes against html5ever 0.38) and adds `markup5ever`, `xml5ever`,
`tendril`, `string_cache`, `string_cache_codegen`, `web_atoms`,
`precomputed-hash`, `new_debug_unreachable` and the `phf` 0.13 family (all
MIT or MIT OR Apache-2.0) to the lock graph. The converter walks the tree
recursively, so `fetch_url` bounds the parsed depth at 512 elements and
converts on a thread with its own 32 MiB stack. URL parsing and IDNA use the
already locked `url` crate. The feature is enabled only by facade features
that already link `plakous-http` (chapter 01 section 12.1), so no
composition-only closure reaches the converter.

### 7.1 Rules

- crates.io is the only allowed registry; unknown registries and unknown Git
  sources are denied;
- wildcard dependency requirements are denied, except a versionless path
  dependency on a repository-only package, which never reaches a published
  manifest;
- a Git dependency requires an immutable revision, a provenance entry, and an
  entry in §7.3;
- yanked and known-vulnerable dependencies fail the supply-chain gate;
- the committed `Cargo.lock` is authoritative for applications and for release
  evidence, and every build command passes `--locked`;
- duplicate versions warn, and must be justified before a release when they
  materially affect size, security, or type interoperability; and
- provider SDKs are protocol references and differential oracles, not default
  production dependencies.

`deny.toml` enforces the mechanical half: `advisories.yanked = "deny"`;
`bans.wildcards = "deny"` with `allow-wildcard-paths = true` and
`multiple-versions = "warn"`;
`sources.unknown-registry` and `unknown-git` both denied with crates.io as the
only allowed registry and no allowed Git source.
Run it when dependencies change:

```sh
just deny   # cargo deny check advisories bans licenses sources
```

The maintenance advisory RUSTSEC-2023-0089 is excepted only for
`atomic-polyfill` 1.0.3 in the all-target lock graph. `monty` 1.2.0 enables
Postcard's default `heapless-cas` feature; `heapless` 0.7.17 selects
`atomic-polyfill` only for AVR, RISC-V 32-bit bare-metal, Thumbv6m, and Xtensa
bare-metal targets. Neither supported product target reaches that dependency.
The workspace checker verifies both supported targets' normal/build dependency
graphs with all features and fails if the dependency becomes reachable or the
check cannot complete. This is not a vulnerability waiver for a shipped
component. Reevaluate the exception when these dependencies or supported
targets change.

The `zip` extraction advisories RUSTSEC-2026-0367, -0368 and -0369 are
excepted only for `zip` 8.6.0, which `ruff_db` 0.0.14 (the Ruff/ty version
Monty 1.2.0 locks) uses to read the bundled typeshed archive from memory; it
never calls the affected extraction APIs. `monty-typeshed` itself uses the
patched `zip` 9 (§7.3). Reevaluate the exception when `ruff_db` moves to
`zip` 9 or starts extracting archives.

### 7.2 Licenses

The workspace is `MIT`; vendored sources and the Codex and pi excerpts named in
the README keep their upstream licenses. `deny.toml` allows Apache-2.0,
BSD-3-Clause, CDLA-Permissive-2.0, ISC, MIT, MIT-0, MPL-2.0, Unicode-3.0, and
Zlib at a 0.8 confidence threshold. The reviewed transitive allowances are:

- **Unicode-3.0** covers Unicode standard tables bundled by otherwise approved
  crates; it does not broaden Plak's own source license.
- **MIT-0** and **Zlib** cover the reviewed transitive graph of the Draft
  2020-12 JSON Schema validator — `borrow-or-share` through `fluent-uri`, and
  `foldhash` through `hashbrown`.
- **MPL-2.0** is an allowance with no current consumer in the lock graph.

`winx` 0.36.4 has a package-and-version-specific allowance for
`Apache-2.0 WITH LLVM-exception`, matching its bundled license. It is the
Windows adapter in Monty's `cap-std` graph, audited by `just deny` even though
Windows is not a supported product platform.
`ar_archive_writer` 0.5.3 has the same allowance: it is the build-time archive
writer of `psm`, reached through Ruff's parser `stacker`. `minicbor` 2.3.0 and
`minicbor-serde` 0.7.1 have package-and-version-specific allowances for the
permissive `BlueOak-1.0.0`: Monty 1.x serializes session dumps through them,
although this adapter never dumps a session.

An exception approves those crates, not additional license kinds or unknown
sources.

### 7.3 Dependency overrides and the Code Mode protocol

One `[patch.crates-io]` redirect steers a dependency to the reviewed set:
`monty-typeshed` → the reviewed local fix: a reproducible typeshed archive
(fixed timestamps, sorted entries) built with `zip` 9. The same change is
prepared for upstream Monty; remove the override once a reviewed upstream
release carries it. No dependency resolves from a Git source.

Code Mode uses the reviewed `vendor/codex-code-mode-protocol` snapshot and the
local `plakous-js-runtime` rquickjs evaluator, which runs only in the
`plakous-js-host` sidecar. No upstream Codex remote
client or official native host binary is part of this execution path.

### 7.4 What is deliberately not depended on

- No general JSON Schema engine and no schema resolver. The executable-tool
  runtime uses the bounded validator owned by `plakous-model`, and there is no
  network or filesystem resolver.
- RFC 8785 byte serialization uses the exact-pinned, locally reviewed
  `serde_json_canonicalizer`. Plakous keeps strict parsing, duplicate-key
  rejection, resource bounds, the deliberately small portable schema language,
  and input/output validation authority.
- SQLite is statically bundled through a pinned `rusqlite` with reviewed compile
  options; there is no system SQLite dependency.
- Provider SDKs are absent from the production graph.
- `syntect` (default syntaxes, `regex-fancy`, no HTML/themes dumps) is the
  TUI highlighter for Compose programs and markdown fences. It is a `plak`
  presentation dependency, not a Plakous engine crate. A keyword list would
  mis-paint template strings and comments; the crate is MIT-licensed and
  matches the MSRV. Default features stay off.

Exact pins (`=`) are used where a version is a reviewed artifact rather than a
range: `filedescriptor`, `jsonwebtoken`, `nix`, `portable-pty`, `rusqlite`,
`serde_json_canonicalizer`, `tracing`, `vt100`, the OpenTelemetry family, and
the external `monty-pool` / `monty-proto` / `monty-types` / `monty-alloc` at
`=1.2.0`.

## 8. Repository settings

Some controls cannot live in the worktree. Authorized signed commits go directly
to `main`, and local verification is the authoritative gate. **The repository
uses local gates and has no CI except the release workflow** (§6.6), which
builds and publishes a pushed tag and checks nothing else. A contributor runs
`just check` before each commit on their own machine, and `just full` runs at
batch and publication boundaries.

This does not weaken release requirements, nor claim that an unprotected branch
is suitable for public collaboration. The desired remote settings are:

- allow only GitHub-authored actions plus exact reviewed third-party actions,
  and require every action pinned to a full commit SHA;
- grant workflow tokens read-only contents permission by default;
- enable dependency alerts, security updates, and private vulnerability
  reporting;
- protect `main` from deletion and force pushes;
- require pull requests, resolved conversations, and status checks before
  merging;
- avoid an impossible self-approval rule while there is one maintainer, and
  require independent review for CODEOWNED policy paths when more than one
  maintainer reviews; and
- never auto-merge a dependency or action update without reviewing the diff and
  the upstream release and security notes.

These are documented desired-state controls, not evidence that the remote
settings have been changed.

Dependency updates are manual: a maintainer raises a version and the change is
verified by `just full` and `cargo deny` before it lands. There is no
automated update service. Routine checks are deterministic and offline except
for advisory acquisition, which refreshes advisory data, the static group's
locked dependency fetch before the structure checkers, and the package group's
archive verification-input prefetch (§6.5). Both dependency preparation paths
try offline first and go online only for missing cache inputs.

## 9. Change requirements

A change to **public semantics, crate boundaries, a persisted or wire format, a
security boundary, or a supported platform** MUST update the section of this
specification that owns the rule, in the same change. There is no changelog and
no separate decision step: this specification is the record of what the system
does, and documentation moves with the behavior it describes. The same applies
to `../user-guide/` when a change alters what an operator types or sees.

Decision history is not tracked in this repository.

Beyond the mechanical gate in chapter 05, three rules constrain every change.
**Backward compatibility is not preserved**: obsolete paths are removed rather
than wrapped in compatibility layers, fallbacks, or migrations, and an unshipped
API or database carries no compatibility claim. **The simplest implementation
that fully meets the current architecture wins**, without speculative
abstraction, configuration, or indirection. **Visibility stays as narrow as it
can be** — private, then `pub(crate)`, then public — with parsing and
presentation kept at the product boundary, so neutral core types never depend on
the CLI or TUI and provider names never enter `plakous-model` or
`plakous-agent`.

Credentials, authorization headers, private session data, and unsanitized
provider fixtures are never committed.

The feature-security digests in `tools/checks/check_features.py` are reviewed
manifest-surface pins, not values to refresh mechanically. After an intentional
Cargo feature or dependency change: run the checker, review the exact manifest
diff, the internal edges, the feature forwarding, and the facade exposure, then
update only the reported changed digest entries and rerun the checker with its
structure unit tests. A digest change is never by itself evidence that the new
surface is acceptable.
