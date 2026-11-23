# Testing and evidence

This chapter states what the repository verifies and what each kind of evidence
can claim. **Evidence binds an exact tuple**: source, binary, host, procedure,
account, provider, model, effort, composition and protocol. **Reachability is
not qualification**: a catalog listing, a nameable symbol, a command that
parses or a route that renders is not a support or correctness promise.

### Binding, upgrade and construction evidence

- **Route and executor contracts.** Data-only builds pin selected/effective
  configuration, model authority, per-tool schemas and bindings, registry,
  projection and plan across native, API and OpenRouter families, root/leaf
  roles and Direct/native/JS/Python presentations. Synthetic platform, time and
  workspace authority enter through normal parsers and builders; never patch
  digests. Repinning requires review. An unchanged executor binding must
  continue accepting the structural subset described by each old schema;
  narrowing requires a binding cut. Distinguish revised and unknown executors
  from provider changes. Host-local file observations are not golden inputs.
- **Persisted binding witnesses.** The predecessor-authored SQLite fixture pins
  native/API root and child bindings, source and input digests, and
  non-dispatching sidecar identities. Linux opens exact locators; other platforms
  check frozen data and the expected execution-binding cut, not native Linux
  composition. Rebuild using the committed recipe in a detached checkout with
  isolated artifacts and immutable dummy sidecars. No private store, credentials
  or inference enter the fixture. Read-only inspection preserves main and WAL
  bytes and uses one runtime snapshot; inactive refusals do not gate. Differential
  preflight separates new live regressions from old failures, inactive rows and
  external availability. Exact reopen cannot substitute for the export carrier
  across state keys.
- **Planned handoff.** Same-key real-binary fixtures exercise bounded drain,
  cancellation and continuation, PID-preserving promotion, pre-surrender
  rollback and once-only terminal publication. Exceeding a drain bound with a
  foreground call or paid compaction must not silently cancel, redispatch or
  surrender it; Stop cannot continue work it stopped. Progress polling joins
  the same computation and deadline. Holds revoke stale ingress, extend
  approval expiry only by parked time and remain outside model-operation
  deadlines; a crash before durable surrender is owner loss, not a handoff.
  Client delivery at the verdict barrier is an attempted write, not receipt.
  Deadline tests belong to the owner of each clock: L0 holds, provider watchdogs
  and process executors are separate evidence.
- **Settled carry.** Install fixtures exercise real host and runner processes,
  named state, paused queues, source refusal or precommit rollback,
  destination-only continuation after surrender, and exact imported successor
  readiness. Remote placement must not consult a same-spelled host checkout.
  Partial, unavailable and abandoned successors stay withheld until their
  own proof; an already-Ready successor keeps its foreground work. The
  runner qualification interface is read-only with a closed allowlist.
  Test-only overrides (`PLAK_TEST_BINDING_CUT_IMAGE`,
  `PLAK_TEST_FORCE_HOST_UPGRADE`, `PLAK_TEST_STATE_KEY`,
  `PLAK_TEST_HOST_UPGRADE_RUNNER_REVISION`) control these cases without bypassing
  custody checks or changing production reads. No fixture proves a live-tree/job
  migration or cross-revision binary pair. Use literal producer/destination
  builds and staging receipts for cross-revision evidence (chapter 02 §4.6).
- **Hosted ports.** A scripted `RuntimeFactory` uses the same construction
  port as production. Admission-scoped observers do not retain the prompt;
  immutable registration is construction-root-local. Switchable test overrides
  remove only their own registration, including out-of-order drops. The tests
  preserve normal admission and publication ordering.
- **External facade.** Compile and run the custom-model/tool workflow,
  cancellation, durable reopen and shutdown from outside the workspace in the
  public contract gate (§1.1). The 30-second lifecycle uses only public APIs;
  all-feature and JavaScript-only consumers require no `[patch]`. This proves
  neither provider admission, sandboxing, evaluator behavior nor distribution.

### Provider wire and continuity evidence

The compiled Codex review compares official `rust-v0.166.0` and
`rust-v0.169.0` and public API documentation, not live 0.169.0 acceptance.
A rotated binding needs fresh catalog/refresh and HTTP/SSE qualification,
including tools, encrypted replay, Lite Code Mode and single-dispatch
compaction. The Platform review requires listing and stateless multi-turn,
tool and compaction checks on `api-openai`. Terra, Cyber and Rosalind remain
`NeedsReview`; offline fail-closed classification needs no paid request.

[Anthropic Platform refusal guidance](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback)
requires discarding partial output, not retrying unchanged context or falling
back. It does not qualify Claude OAuth. The pinned native interactive
Claude Code 2.1.284 macOS arm64 executable (SHA-256
`50a14c2f50f56668380fdda490167f1d3630d5cc18fb8aed3073c2c7ea7314fe`)
lists Sonnet 5.5 and low–max effort. First-turn captures at low, medium and max
and short authenticated interactive turns do not prove server-side effective
effort, multi-turn thinking replay or native PTC. Native costume/attribution
uses separately pinned, synthetic interactive captures; source-derived and
native-executed checksum evidence are distinct (§6).

Refusal fixtures distinguish a physically closed malformed client proposal
from an open block or a possible hosted effect. Closed, undispatched local
and MCP proposals may be discarded, but `Pending`, `Completed`, `Failed` and
provider-hosted calls cannot be revoked. Preserve identity, usage and stop
facts; publish no invented output/results and allow an explicit next turn
without replay. Continuous Claude requests retain attributable provider
continuation after exact reopen, reject ambiguous, imported and foreign
provenance, and tie the HTTP session header to the logical UUID, not the
per-dispatch request UUID. None of this shows why a provider refused; a live
file-work oracle checks the file independently of the protocol terminal.

### Process lifetime and recovery regressions

| Family | What the evidence must pin |
| --- | --- |
| Admission and release | Preparation deadlines precede auth and forbid later dispatch. Exact lease cleanup is idempotent, batch atomic and fenced against renewed, reacquired or foreign grants. Release joins descendants and stops before sealing; interrupt alone does not stop promoted jobs. Live attempts or jobs prevent submission. |
| Continuations and waits | Idle notices are home-policy controlled, content-free and suppressible by interrupt. Run bounds count admitted turns; wait/timeout/SIGINT are proved with real jobs, joined cleanup, exact reopen and once-only stdout. Polls are wake-driven and leave the async/blocking pools responsive; retries do not flood snapshots. Durable non-mail receipts are not wake tokens. |
| Foreground and job ledger | Ownership is branch-local. Foreground transfer precedes promoted transfer; only matching calls stop, and an unknown outcome cannot masquerade as exit. Retain attributed terminals/counters across carry and folds until atomic consumption, reserve terminal capacity before promotion and preserve ordered job transitions under backpressure. Idle wake and model notice consumption are separate. |
| Steering and cells | Only safe boundaries preempt. Superseded unstarted calls have durable no-dispatch proof; nested mutations settle once, retain results and never inherit top-level control variants. Parked/cancelled observations resume the same cell or return its buffered terminal, leaving the original delivery deadline and bounded output intact. Named state publishes old or complete new state at result commit; no partial maps or replay on reopen. |
| Recovery and compaction | Recorded decisions finish without redispatch. Fenced effects settle outcome-unknown with exactly one typed terminal and honest claim/cause; queue drain is explicitly resumed. Same-fence lease revival cannot outrun foreign takeover. Compaction limits paid dispatch, preserves valid reasoning only on unchanged prefixes, measures the actual sanitized tail, and keeps operations/counters through exact reopen. |
| Transport versus capacity | Test-I/O allocation refusal is injected before a store mutation, not by filling disk or using transport injection. Assert unchanged heads on refusal, exact later publication, retained unsaved output and intent identities, and no repeated paid call; SQLite generic IOERR is not inferred to be EDQUOT. Clock-based cases advance the owner clock without substituting sleep or extending authority. |
| Provider process vocabularies | Grok multi-await has one deadline and consumes only terminals; repeat snapshots, natural exits and stop/steer semantics stay distinct. Claude's native pack exposes exactly Bash, Edit, Read, TaskStop and Write. TaskStop takes only the string task ID; output uses the issued path via Read, not a retired output or shell-ID alias. Native schemas are compared against sanitized captures; truthful presented descriptions are not claimed as byte copies. |

Runner-side platform tests distinguish a live process from a PID or stream
terminal. A missed observation cannot authorize a signal; exact session/group
proof, lost-instance receipts, bounded cleanup scans and explicit acknowledged
loss are different evidence. Serial PTY and durability host/runner cuts stay
outside offline parallel. They require both supported operating systems;
Linux cannot qualify macOS supervision or the browser/client scheduler.
Operator-terminal, Files, account and console evidence is grouped in §6.4.

### The Harbor bakeoff plane

`eval/` is opt-in, paid harness comparison using the same `plakd run` binary.
Neither campaigns nor the zero-inference `just harbor test` suite are gate or
product qualification evidence. `eval/README.md` owns datasets, architecture-
matched pinned comparators, private digest-reviewed plans, calibration, A/A
and campaign recipes. Use interactive subscription OAuth for native Claude;
do not substitute print mode.

Calibration is unscored. A scored campaign must carry its required source,
binary and platform-bound preparation receipts. Recorders verify upstream TLS
and export structural fields and digests only; plakd uses its own wire capture
and exact-URL routes. Operators must prevent cross-host lease reuse. Incomplete
wire evidence and retry lower bounds are never complete/exact observations.
Only the documented Claude billing-nonce projection permits instruction
normalization; every other field remains pinned.

## 1. Gate layers

The authoritative gate is `tools/gate/dev-check.sh`, in five tiers plus
explicit step-group and opt-in soak modes. It is Cargo tooling plus tests plus
thin structure checks. Process paperwork — requirement digests, provenance
ledgers, documentation link matrices, timing ledgers and source-size reports —
is not part of it.

```text
tools/gate/dev-check.sh --fast [--rdeps] [PKG...] # just fast
tools/gate/dev-check.sh --batch          # just batch (also the script default)
tools/gate/dev-check.sh --batch-mac      # just batch-mac
tools/gate/dev-check.sh --install        # just install (without a snapshot flavor)
tools/gate/dev-check.sh --install-checks # just install-checks: unscoped Mac tests
tools/gate/dev-check.sh --full           # just full
tools/gate/dev-check.sh --steps GROUP,... # just steps GROUP,...
```

| Group (canonical order) | Coverage |
| --- | --- |
| `static` | Workspace all-features, all-targets clippy with warnings denied. Except in Mac batch, also Rust formatting, `ruff check` and `ruff format --check` over `tools/`, `tests/tooling`, `tests/live` and `eval/`, locked dependency fetch (offline first), and `check_workspace.py` and `check_features.py`. The feature checker's library builds use a separate target directory. Full also audits the publication evidence allowlist (chapter 06 §6.5) |
| `builds` | Link plakd in its release resolution and with maintainer features, build the two sidecars, and verify their linkage against the maintainer binary; rustc warnings denied |
| `rustdoc` | Workspace all-features documentation with warnings denied |
| `rustdoc-alone` | Documentation of each supported package alone in its default resolution, warnings denied, in a dedicated target directory |
| `standalone` | Clippy each of the publish-plan crates alone in its default resolution (`--lib --bins`, `--bins` for the binary-only Python worker, or `--all-targets` for supported packages), warnings denied, in a dedicated target directory. Check portable wasm32 libraries there (chapter 06 §7.5), and package the staged publication cohort with `cargo package --locked --offline --no-verify` in a separate `package-lite` target. When archive verification is selected on a repository tree it already supplies the packaging witness, so no separate package-lite run is needed |
| `contracts` | Positive and negative public compile contracts |
| `tooling` | Python tooling self-tests, including a test-only socket driver and the user-guide hello/directory/attach transcript against an isolated real host, requiring exact wire-version refusal. Batch always runs the tree readers and selects the rest by changed path |
| `tests-parallel` | Nextest `offline` profile |
| `tests-durability` | Nextest `durability` profile: core crash-law rows and the SQLite SIGKILL campaign in a host-scaled weighted pool |
| `tests-serial` | Nextest `serial` profile: wall-clock budgets, four threads on Linux and one on macOS |
| `tests-install` | Complete install profile in a host-scaled weighted pool, including two exclusive indexed crash-sweep campaigns |
| `package` | `tools/release/publish.sh --gate`: prefetch archive verification inputs offline first, then package and verify the staged cohort strictly offline (chapter 06 §6.5); requires a repository tree |

| Tier | Groups |
| --- | --- |
| fast | `static`, scoped `tests-parallel` if any package is touched |
| batch | `static`, `standalone`, `contracts`, path-scoped `tooling`, scoped `tests-parallel`, core `tests-durability` and `tests-serial` |
| batch-mac | Workspace clippy, a shared test build and three platform-sensitive passes: parallel, durability, serial |
| install | `builds`, `rustdoc`, full `tooling`, full `tests-parallel`, `tests-install`. The named `just install-checks` supplement runs all four Mac test lanes, without packaging or batch scoping |
| full | Every group and both ignored retirement soaks |

The tiers are not nested. Integration, exact-tree installation and release
requirements are in chapter 06 §8. A passing full run that starts and ends
on the same clean tree records that tree id in
`$CARGO_TARGET_DIR/gate-evidence/full` (default `target/`). Clean Git roots
include primary checkouts and linked worktrees whose `.git` is a gitdir
file; both must match Git's reported repository root. An unconflicted
colocated JJ root is also eligible, but a secondary JJ workspace is not.
A passing run that includes archive verification on an eligible tree
(full, or `--steps package`) also writes `gate-evidence/package`. A run that
skips a check names the skip in its final summary and writes neither file.
Chapter 06 §6.5 specifies when evidence covers later trees.

`--steps` runs its named groups and rejects unknown or empty groups. When
given a batch base it shares the parallel and tooling batch scoping policy.
It never adds the install profile implicitly; an explicit `tests-install`
always runs the full profile. Test lanes and tooling still prepare the shared
test artifacts. Independent runners use separate checkout, target and scratch
roots. Each group prints a `== group: <name> ==` banner and a
`== time: <name> <seconds> s ==` line. Separate time lines identify test
preparation and package-lite. Standalone and public compile contracts run
under nice in targets separate from workspace test preparation, subject to
the Cargo jobs budget. Package-lite uses a separate target and finishes
before the test build, so it does not reserve a compile slot during it.
In a combined Linux batch
with sufficient build slots, the feature checker and standalone lint run
in sequence in the standalone target, reusing matching Cargo artifacts
without unifying their feature resolutions. This chain starts beside
workspace clippy and continues during the test build.
Clippy finishes first because it shares the test build's target directory.
Build and rustdoc groups that use the main target also finish before the test
build, so they cannot overwrite its executables. The gate joins all these
checks before starting tests. Linux batch can run tooling alongside the
parallel lane; both finish before any durability, serial or install tests.
Overlapped groups replay complete logs and verdicts in canonical order.

After argument and prerequisite validation the gate attempts every independent
selected group even if an earlier group fails. Within `static` it collects
formatting, fetch, checker and clippy failures; other groups may stop on a
failed command. A failed test prerequisite prevents its dependent groups from
running. The summary names failures and returns nonzero.

Worker branches iterate and finish with `just fast <packages>` plus the
groups their change needs. A change to an install-family test or code it
exercises, including crash-sweep shapes, requires `just steps tests-install`
before handoff. Fast uses packages inferred from the Git working
tree against HEAD, or explicitly named instead. At a JJ root, shared
root-checked listings use `builtin_immutable_heads()..@`; inside a plakd copy
they diff `plak_base()` to `@`. `PLAK_FAST_BASE` overrides those defaults.
The fast parallel lane defaults to the touched packages' own tests;
`--rdeps` includes dependent tests as well. Worker clippy always
includes reverse dependencies. Format, static structure and feature-closure
checks stay global; fast narrows the facade and hosted profile compilations.

Batch runners set `PLAK_BATCH_BASE` to the commit immediately before the batch.
With that base, the gate maps the working-tree diff, including non-ignored
untracked files, to the longest-prefix package owners. Ignored proptest
seeds on persistent runners are not batch inputs. Root Cargo and nextest
configuration, shared test-environment tools and pins, `vendor/`, and loose
shared test fixtures select the workspace. The MCP fixture server is a
package, not a loose fixture. Configuration reference inputs outside the
workspace select plakd. An explicit diff with no Rust inputs selects no
parallel tests. Without `PLAK_BATCH_BASE`, both named batch modes use
`all()` as their package scope, even on a dirty checkout or source export.
Batch never consults `PLAK_FAST_BASE` or silently uses an ancestor repository.
The named install modes and default `just musl native` ignore batch scoping.

The batch parallel lane uses `-E 'rdeps(=<pkg>) | ...'` with extra consumers
of changed runtime binaries or protocols: cell and the Python and JavaScript
workers select the facade, hosted and plakd; plakd selects the standalone
crash harness; the MCP fake server selects MCP, hosted and plakd. The runtime
edges are transitive, so both cell workers and the MCP fixture also select the
crash harness. A direct or transitive Cargo input to those binaries can
select their consumers too.
The Mac intersects that same scope with the platform-sensitive predicates in
`tools/gate/mac-selection.filter`: F for parallel and durability, F_SERIAL
for serial. Nextest intersects each expression with the lane profile and an
empty intersection passes. Neither batch runs the install profile.
The Linux install tier, Mac install checks and full each run the complete
install profile regardless of the changed paths. Main can carry an
install-profile failure that the install tier catches before production.

Batch tooling always runs `test_host_client`, `test_nextest_lanes`,
`test_pinned_callback_ports`, `test_configuration_placement`,
`test_uuid_features`, `test_portability_fence`, `test_policy_validators`,
`test_agent_kind_docs`, `test_artifact_linkage`, `test_review_expiry`,
`test_live_jobs` and `test_check_prod_loc`.
Changing anything below `tools/`, `tests/tooling/`, `tests/live/`, `eval/`,
`.config/`, or `tests/fixtures/`, or changing `Justfile`, selects the entire
tooling suite. An unknown scope, install, full, or an explicit tooling step
without a batch base also selects the entire suite.

Public compile contracts isolate their feature graphs from workspace artifacts
in a persistent `.public-contracts/<target-dir-name>/<target-triple>`
directory beside `CARGO_TARGET_DIR` (default target name `target`). The
triple is `CARGO_BUILD_TARGET` or rustc's host triple. The gitignore keeps the
cache through checkout cleanup; `PLAK_PUBLIC_CONTRACTS_DIR` overrides the
cache root, with the triple still appended. Contracts retain the runner's
Cargo incremental and compiler-cache settings. The pinned musl container uses
`/qual/target/public-contracts-<image-tag>` as that root. No diagnostic pin
is relaxed to keep builds warm.

The workspace test build uses `--workspace --all-features` and supplies the
real worker and host artifacts; nextest reuses that feature resolution.
Standalone lints default closures without workspace feature unification.
The install builds cover code generation and linkage that clippy cannot
prove. The portable target is checked by standalone; the structural
dependency fence for that target still runs in `static`. The preloaded musl
qualification container may skip the wasm compile if its rustc target is
absent, but the native musl gate cannot skip it (chapter 06 §7.5).

`-D warnings` is appended to the rustflags Cargo will actually pass. Cargo
keeps one source, in order: `CARGO_ENCODED_RUSTFLAGS`, `RUSTFLAGS`, then all
matching `target.<triple>` and `target.<cfg>` rustflags including
`CARGO_TARGET_<triple>_RUSTFLAGS`, then `build.rustflags`.
`CARGO_ENCODED_RUSTFLAGS`, when set, is extended with the encoded flag.
Otherwise `RUSTFLAGS`, when set, is extended and still replaces configured
rustflags, matching Cargo. Otherwise `CARGO_TARGET_<triple>_RUSTFLAGS`, when
set, is extended; it merges with the file's matching target rustflags and
ranks below those two variables. When none of those variables is set, that
build's `RUSTFLAGS` are the configured `target.<triple>.rustflags` and
matching `target.<cfg>.rustflags`, or else `build.rustflags` (the file value plus
`CARGO_BUILD_RUSTFLAGS`, which Cargo merges after it), plus
`-D warnings`. The addition is not exported to later steps, so the
all-features test artifacts are not rebuilt. A linker configured in Cargo's
rustflags therefore stays on the builds group.
Registry dependencies stay `--cap-lints allow`. A
warning that exists only in a default or maintainer closure, and is hidden from
`--all-features` clippy, fails this group.

Shared checks run once per integration batch, on Linux. Workspace rustdoc
runs at install, and full also runs standalone rustdoc. The native musl
recipe defaults to complete native build and offline test coverage;
`just musl container` runs the full tier in the pinned offline musl
container before publication (chapter 06 §6.1). Both musl scripts set
`CARGO_BUILD_TARGET` to `x86_64-unknown-linux-musl`; the gate passes an
explicit `--target` to every compile and nextest invocation, and subprocess
Cargo checks inherit that target. The portable-crate wasm library check is
the exception: it sets `CARGO_BUILD_TARGET` and `--target` to
`wasm32-unknown-unknown` for that command only (chapter 06 §7.5). A GNU-host
gate does not qualify the supported musl platform.

`tools/gate/test-env.sh` is the one test environment. It exports `TMPDIR` from
`tools/gate/test-tmpdir.sh`, `PLAK_HOME` (`$TMPDIR/plak-isolated-home`, mode
0700), an empty `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_NOSYSTEM=1`. Its shell and
command forms also unset the test-hook overrides `PLAK_TEST_SESSION_LEASE_SECONDS`,
`PLAK_TEST_CELL_HELLO_MS` and `PLAK_TEST_HOST_IDLE_EXIT_SECONDS` and the provider keys `OPENAI_API_KEY`,
`XAI_API_KEY`, `ANTHROPIC_API_KEY` and `OPENROUTER_API_KEY`, so an operator's
exported key never reaches an offline test. Every form also puts
`$TMPDIR/plak-test-jj`, which holds only a `jj` link to the jj the tools check
uses (chapter 06 §6), first on `PATH`: `PLAK_TEST_JJ` when set, else, with no
`jj` on `PATH`, the newest pinned jj that `just dev-tools install` unpacked. Its `--json` form names only the variables it
exports, `PATH` included when it changed; the live drivers keep their real keys. Its
`--unset-names` form prints the names the other forms unset, and the offline
drivers that take `--json` (`crash-sweep.py`, `jj-tools.py compat`,
`weighted-lanes.py`) drop exactly
those, so no driver keeps a list of its own. `dev-check.sh`
(with `--fresh`), `just dev coverage`, `perf-smoke.sh`, `crash-sweep.py`,
`jj-tools.py compat` and the live drivers use it. `just nextest` runs the
pinned cargo-nextest under it and passes recipe arguments as positional
parameters, so a filter expression reaches nextest as one argument. On Linux
the selector is a per-checkout
directory under `/dev/shm`; on macOS a per-checkout directory under
`/private/tmp`, so neither lies under the operator's `HOME`, where a spawned
binary's project discovery could reach the real `~/.plak`. The directory is
named `plak-test-<uid>-<cksum of the checkout path>`, must be owned by the
caller and mode `0700`, and must resolve to itself with no symlinked
component; `dev-check.sh` empties it at the start of each run. When the shared
root or the directory fails those checks, or on another system, the selector
falls back to `target/plak-test-tmp` in the checkout. Linux `fsync` is a real
device flush that can exceed the stop, cancel and quit bounds under gate load.
macOS `fsync` does not flush the device, so the two platforms only agree when
the Linux scratch is RAM-backed. Maintainers may select another disposable
filesystem with `PLAK_TEST_TMPDIR`; the selector resolves its physical path
before `TMPDIR` is exported. This changes test storage placement, not
deadlines or assertions, and must be recorded when interpreting
filesystem-sensitive results. On macOS `/tmp` and `/var` are symlinks, the
SQLite stores open with `SQLITE_OPEN_NOFOLLOW`, and the credential store walks
refuse symlinked parents, so a default temporary root makes every host test
that opens `runtime.sqlite3` fail with `SQLITE_CANTOPEN_SYMLINK`. `just nextest`
selects that physical, non-symlinked `TMPDIR` outside `HOME`.

The gate applies `PLAK_HOME` on nextest lanes only, so other cargo commands do
not inherit a product home. Offline tests must not create the operator's
`~/.plak`. Product home resolution consults `PLAK_HOME` before `$HOME/.plak`,
so a test, or a `plakd` it spawns, that forgets to choose a home writes that
scratch directory instead of the real one. `HOME` stays the operator's, because
Cargo reads it.

Git in tests reads no operator or system configuration. The helper truncates
`$TMPDIR/gitconfig` on every call. `dev-check.sh` exports the git variables for
every group, so the nextest lanes, the crash sweep and the Python tooling
suite inherit them. An operator's global
`commit.gpgsign`, hooks path, aliases or identity cannot reach a fixture. A test
that commits sets its identity on its own command (`-c user.name=...
-c user.email=...`) or in its fixture repository's local config, never through
the global config. The live drivers give their own fixture git and scratch the same isolation.
`isolate_test_environment` snapshots the process environment, then exports the
helper onto the driver. A `plakd` child is built from that snapshot
(`Env.environ`'s `source`, and the other child launches), so it does not
inherit the per-checkout `TMPDIR` (a `--fresh` gate empties that directory)
or the empty git config. The child still sets its own `PLAK_HOME`. `just nextest`
exports all four for the test process.

| Layer | Command | What it proves |
| --- | --- | --- |
| Formatting | `cargo fmt --all --check` | The tree is byte-identical to `rustfmt` output under the pinned toolchain |
| Lints | `cargo clippy --locked --workspace --all-features --all-targets -- -D warnings` | Every target of every package, all features on, no warnings. The workspace lint table denies `missing_docs`, `unwrap_used`, `expect_used`, `dbg_macro`, `todo`, `unimplemented`, and broken intra-doc links, and forbids `unsafe_code` |
| Standalone compilation and archives | `just steps standalone` | The publishable crates lint in their own default resolution, portable libraries compile for wasm, and the staged tree can form the cohort archives |
| Install builds | `just steps builds` | The release and maintainer resolutions of plakd and their sidecars link with the expected native-library split; warnings in these builds are errors |
| Portable wasm libraries | `cargo check --locked -p <crate> --lib --target wasm32-unknown-unknown` for each name from `check_workspace.py --wasm-libraries`, with `-D warnings` | The portable set compiles as a library for `wasm32-unknown-unknown` (chapter 06 §7.5). Currently that set is `plakous-model`, `plakous-runner-protocol`, `plakous-tools` and `plakous-agent`, whose default features are portable, and the list adds the default library of `plakous-store`, whose `sqlite` feature keeps it off the fenced set. This is not a supported product target, and the check proves compilation, not run-time behavior. The musl qualification container skips the check while its preloaded image lacks the rustc target; the summary names the skip and the run records no gate evidence |
| Docs | `just steps rustdoc` at install; `just steps rustdoc,rustdoc-alone` at RC | Broken intra-doc links, links into private items and bare URLs are errors. Standalone docs build each supported package in its own default resolution |
| Offline tests | `just steps tests-parallel,tests-durability,tests-serial,tests-install` (the gate supplies the host-scaled durability configuration) | The whole workspace's deterministic behavior, with no network and no credentials |
| Public compile contracts | `python3 tools/gate/check_public_compile.py` | Supported inventories still compile from outside, and withheld authority still fails to compile |
| Workspace structure | `python3 tools/gate/check_workspace.py` | Every package at the path its `[package.metadata.plakous] layer` prescribes, its support class legal for that layer, every dependency edge admitted by the layer matrix or an explicit exception, neutral packages separate from providers, the facade as the only Plakous surface of host/bin sources, the CLI ambient surface, the `plakd`/`plakous-js-host` build split, and tracked-file hygiene: no tracked or untracked unignored text file carries an absolute home path outside a short list of generic placeholder users, or a private host or maintainer tool name listed one per line in the optional untracked `docs/local/hygiene-private-names.txt` (absent, that name check is skipped; the tree carries neither the names nor their digests); no per-file exemptions. The same checker fences `PORTABLE_CRATES` (chapter 06 section 7.5): each listed crate's normal closure, at default features and at `--all-features`, must not carry a reviewed native-only crate (including `libc`) or Tokio feature, resolved for `wasm32-unknown-unknown` rather than the host, and a failure names the crate, the dependency and the path that pulls it in |
| Feature closures | `python3 tools/gate/check_features.py` | Per-package feature closures (2n+3 witnesses: none, each singleton, full minus each feature, default and full; never a cross-product), statically, then `cargo check --lib` of thirteen standalone library profiles (eleven facade profiles, including API-only and standalone provider and tool profiles; hosted without optional features and with all features), warnings denied (spec 06), plus reviewed manifest-surface digests, locally declared UUID generation features, and test-only dependency features (transport fault injection) absent from a default `plakd` and present under `test-support` |
| Tooling tests | `python3 tools/gate/run_tooling_tests.py` | Offline tests over the gate runner, checkers, build, live-driver and measurement helpers. Up to four `python3 -m unittest` processes run at once; the mock gate-driver module is divided by test case, and other selected modules run whole |

Nested-failure regressions cover exact successful string delivery, caught and
uncaught JavaScript rejection, `Promise.allSettled`, retained earlier output,
correlated acceptance, and joined shutdown through both embedded and sidecar
engines, all through the `plakous-cell` client. The cell protocol has its own
offline suite: frame round trips, lengths refused before allocation, truncated
and unterminated frames, and scripted workers proving that every interleaving
of nested calls, delivery verdicts, output and terminals ends a cell with
exactly one terminal and reports `Accepted` only on the worker's own verdict.
Both real-worker `CellSession` suites deliver a depth-40 nested result and turn
depths 41 and 50 into catchable per-call rejections, then run another cell on
the same session. The JavaScript suite carries a depth-40 stored value through
the next prelude and refuses depths 41, 50 and 128 before terminal framing,
preserving earlier valid writes after both caught and uncaught store errors.
The stored-value protocol tests also reject depths 41 and 50 in preludes and
both terminal variants in both serialization directions, retaining a typed
`StoredValueDepthError`; JavaScript catches the corresponding `RangeError`.
The private-driver scheduler additionally exercises failed read-only
and partially effective mutating children, later successful mutation, rejected
invalid arguments, finalized child records, and joined cancellation. Canonical
projection tests distinguish ordinary tool failure from infrastructure failure.
Every tier that runs Rust tests takes the `plakous-py-host` and
`plakous-js-host` binaries from the workspace test build (`cargo test --no-run
--workspace --all-features`) and obtains their executable paths from Cargo's
artifact records, respecting Cargo target-directory and build-target
configuration. The MCP fixture server is selected from the same records and
passed as `PLAK_MCP_SERVER`; tests never infer its location from Cargo's
intermediate test-executable layout. The product's artifact path is passed as
`PLAK_TEST_BINARY` for the crash harness. Explicit `PLAK_PY_HOST`, `PLAK_JS_HOST`,
`PLAK_MCP_SERVER` and `PLAK_TEST_BINARY` overrides instead select existing
executables (all four are required to skip the test build); missing, empty,
directory, or non-executable paths fail the gate. All nextest lanes receive
their absolute paths, so real Python tests
cannot silently take their missing-worker early return. A worker that cannot
run or speak the protocol fails those tests. Manually invoked Rust tests
without a worker retain their existing limited, potentially skipped coverage.
`just nextest` only isolates the environment; manual full-suite runs must set
all four executable paths themselves. Manually invoked MCP fixture tests
require `PLAK_MCP_SERVER` to name the built workspace fixture binary.
Standalone product crash-harness tests likewise require `PLAK_TEST_BINARY`;
neither fixture lookup walks the intermediate test-binary directory tree.
Standalone crash sweeps and weighted-lane trials likewise build the workspace,
select all four non-test executable artifacts from that one Cargo JSON capture,
and replace any inherited fixture paths before running or listing tests. A
missing or unusable artifact refuses the run instead of using stale siblings.

Python regressions distinguish queue handoff from correlated worker
acceptance, lost receipts from successful delivery, and rejected values
from a delivered Python exception. Offline conformance adapts micro-monty's
parallel weather, retry/permanent-error and stop-early pagination tasks to
the real worker and cell protocol. It holds replies until the whole gather
batch is admitted, exercises typed errors and a retry, and verifies
structured values and unparsed text. Additional real worker cases exercise
typed record/byte results, ordered and capped print output, persistent versus
fresh globals, SDK named state, catchable memory errors, the compute watchdog,
and termination with joined process cleanup. The worker tests also hold a
result until its future is awaited or the terminal declines it, cross the
cumulative print cap at a UTF-8 boundary, and keep 200 MiB live while a
native copy hits the charged heap limit. A debug-only allocator crash cut
forces a hard-cap refusal before handing a nested reply to the engine.
Real-worker exception cases
check deterministic cell ordinals, chained causes and notes, the size and
tail of a mutual-recursion report, and the source line, class and message of
a rejected tool. The worker must omit the exact generated prelude frame
without changing any other traceback text.
The durability profile's `cell_worker_delivery_crash_cuts_preserve_only_proven_acceptance`
kills both real workers at test-only boundaries before result handoff and
after their acceptance receipts but before terminal publication. The first leaves
delivery unknown, the second retains acceptance, neither produces a cell terminal
or permits source/nested-call replay, and cleanup cannot relabel loss as joined.
Both workers expose `PLAK_TEST_CELL_CUT=before_delivery|after_accepted` and
`PLAK_TEST_CELL_CUT_READY=<sentinel>` only with `test-hooks` and debug
assertions enabled; ordinary and release builds do not read those variables
or create sentinels. The boundary parks after reading `NestedResult` but
before interpreter handoff, or after flushing `Delivery{Accepted}` and before
publishing `CellDone`. The durability test
`real_cell_worker_crash_recovery_boundaries` kills each real interpreter at
both boundaries, reopens its SQLite session, and requires no source or nested
tool replay, exactly one root terminal, and the persisted honest
`RecoveryRequired` diagnostic rather than a fabricated successful cell result.
The reopened canonical history has no successful outer exec result; the exact
nested effect remains completed without replay at both cuts. Canonical child
completion precedes delivery and is not evidence of interpreter acceptance.
The direct real-worker receipt tests separately prove unknown delivery before
handoff and acceptance after the correlated receipt. Both worker paths come
from the gate's Cargo artifact manifest or
explicit overrides, and a debug-only `--test-cell-hooks` probe refuses workers
without the required hooks before the crash test starts.
These interpreter cuts are in the durability lane, not the indexed SQLite
commit sweep. The private-driver
scheduler also withholds its first delivery receipt until the second result
arrives, while checking canonical child finalization and joined cancellation.
Owner-transfer regressions cover a committed tool terminal followed by refused
child-edge publication, preserving definite evidence without result delivery,
sibling effects, or replay. Injected panics before source admission, after
source admission, and after cell acknowledgement preserve explicit recovery
obligations when the extracted resource pair is lost. Nested read-only and
mutating permit waits recheck an expired store clock before executor entry,
including exact duplicate responders. Persistent-interpreter coverage runs a
successful compaction between cells, retaining the same generation and stored
value while joining final session cleanup once. These deterministic fixtures
complement the real process-nested stack and cancellation witnesses; they do
not claim real-engine injected lease timing.

The repository's `crates/plakous/tests/agent_scripted_quickstart.rs` is an offline
integration test:
it constructs through the curated facade, completes its scripted turn and joins
shutdown. This verifies the executable example, not only its imports or syntax;
it does not claim live-provider embedding qualification.

Lease-domain regressions exercise store authority time directly and refuse
equality, unavailable clocks and mismatched domains at final admission,
and preserve exact historical receipts after a simulated reboot while requiring
higher replacement fences. SQLite tests reject established same-boot foreign scopes without
changing database/WAL bytes and prevent an old handle from adopting the new
binding. Independent child processes exercise the default host clock against a
shared database. The lingering-owner process test excludes the product WAL
writer while suspending the owner and waits for its stopped acknowledgement
before releasing that exclusion: a stopped SQLite writer would retain its lock
indefinitely, which is not lease-fencing behavior. A separate contention test
requires binding writes to refuse without partial state and succeed after the
writer releases; no timeout or stale-owner oracle is relaxed.
These deterministic and process tests do not simulate a real
host suspend, privileged namespace transition or reboot; Apple Silicon execution
remains a separate platform qualification step.

`nested_delivery_panics_preserve_remaining_shutdown_stages_and_prior_failures`
queues one result and an exact duplicate. The original responder panics during
execution; the duplicate waits for integration shutdown before panicking during
final settlement. The fixture proves one recovery terminal and executor entry,
no accepted result or replay, later lease release, store flush and event closure,
and preservation of a prior integration-shutdown failure. Tool-task shutdown and
heartbeat-join ordering are source-audited on this path.

Runtime organization checks preserve public compile contracts, test basenames,
wire identities and golden assertions. Private durable fixtures live beneath
the durable owner so tests can inspect its nonce and compatibility state without
exposing live fields. The full agent suite and real executor-stack witness remain
required after runtime module moves.

`just fmt` is the formatter: `cargo fmt --all`, then `ruff format` over
`tools/`, `tests/tooling`, `tests/live` and `eval/`. `just clippy` lints the
all-features workspace. `just steps standalone` lints the publishable packages
alone. `just steps rustdoc` documents the workspace; `rustdoc-alone` adds
the supported packages' default-resolution docs.
`cargo-nextest` is required by every tier: if it is absent the gate exits 2 rather than falling back to `cargo test`, because a plain
workspace `cargo test` would skip the serial budget lane and the nextest test groups.

The file-read regression suite checks exact empty, LF, CRLF, unterminated,
Unicode, and metadata-looking content; selected-line pagination; Claude Direct
cat-n projection with byte-exact round trips for empty, LF, CRLF, no-final-LF,
mixed-ending, and Unicode content while nested JSON stays unchanged; and
structured observation-budget truncation. Local
composition must receive neutral JSON rather than a Direct display header.

Product-store ownership evidence drives the real session-list command and
attach configuration constructor across separate state-root requests and a
worker thread, checking that they use one lazily opened connection. Replacement
coverage drives the command after replacing the product home. SQLite contract
tests cover failed-open retry and rollback after interruption between branch
writes, including continued use of the same service. The identical Memory/SQLite
conformance cases also reject cross-session and retired queue identities,
require consecutive queue edits, and prove failed save/prepare and interrupted
branch publication reserve no identities and advance no caller revisions.
Shared load cases cover every delivery and workflow state, queue-only and empty
records, and both valid stored drain modes. They require paused recovery only for
outstanding work, stable repeated loads and listings, and an unchanged CAS token
until an explicit save. Terminal delivery tests persist nonempty command bodies
through both stores and compare their complete loaded records, reject a body
that contradicts its digest before compaction on save and both prepare paths,
including unchanged rows, revisions, and caller records after refusal and valid
retries by either the original owner or a different owner of the new identities, and prove exact terminal replay
survives receipt eviction while changed replay is refused.

Result-delivery tests cover quiet-window idle admission on results and questions,
coalescing, non-waking findings, disabled and interrupted hosts, queued-prompt
precedence, 30 consecutive wakes with fresh mail and process completions, and
post-terminal delivery of two jobs finishing 10 s apart followed by a child
result after 24 consecutive wakes, including quiet-window coalescing and no
leftovers for user admission. They cover interrupt suppression until a user prompt,
run continuation beyond 24 host-authored turns, and exact-once delivery after
clean close and reopen. They also cover bounded
prompt backpressure with unread results retained, and cuts after journal
preparation and after accepted admission but before tree acknowledgement.
Nested-parent context replay includes newer mail arriving between preparation
and retry, exact prompt conflicts, blocked competing commands, reconstructed
tree state, and repeated acknowledgement. A real SQLite child reopen covers the cut
after durable turn binding but before result acknowledgement, verifies the production
recovery path consumes the exact batch, and proves provider evidence remains unchanged.
Real hosted one-shot and authorized submit-harness turns verify context reaches admitted
commands. SQLite product-store coverage binds
result batches into immutable delivery identity and refuses changed message ids.
Mailbox tests cover unread-only listing and bounded read history, full 2048-byte
steer and followup bodies, exact mail returned and acknowledged by waits,
non-repeating mail wakes, wake reasons, omitted running usage, and unread mail
retained after a failed receipt publish. They preserve numeric arrival order
across reopen and retain terminal attempt output when the global queued-mail
byte ceiling refuses a result push.

Account workflow tests drive command dispatch with a writer-free `AccountOps`
double and injected terminal-machine effects. Automatic completion leaves
stdin unread even when browser launch fails; paste completion follows the
challenge's exact mode. Failed, cancelled, timed-out, and SSH-refused attempts
never retry or switch routes. Owned-PTY subprocess tests verify hidden paste,
retained cooked editing and signals, and exact terminal/file-flag restoration
after success, oversized input, Ctrl-C, and panic. A refusing reader proves the
raw byte bound stops consumption even for endless whitespace; injected
clipboard doubles cover both `c` and `copy`, unavailable clipboard guidance,
and subsequent paste without launching a platform clipboard process. One-shot
execution has no fake method on the account port; its evidence comes from the
hosted driver and actual one-shot adapter.

Workspace-cleanup regressions withhold removal acknowledgement, reopen the
prepared `discarding` row, and refuse duplicate dispatch. Injected unmint and
unpin failures retain the copy or pin, publish recovery rather than discarded
success, and prevent disposal of unresolved seal/session work. Both memory and
SQLite persistence paths verify the pending row and exact terminal receipt.

Child-startup regressions exercise reservation before command acknowledgement,
close/shutdown joining, cancellation before and after live ownership transfer,
late release, a lost acknowledgement deadline, and failed release. They assert
that unresolved work remains reserved and unsealed, durable recovery survives
reload, and exact spawn replay never invokes the binder again. A composed
hosted regression creates a real SQLite-backed session, admits and physically
dispatches one offline provider request, withholds acknowledgement delivery,
and crosses the caller deadline. It then proves close cannot seal the active
copy, late acknowledgement releases the owned attachment, and exact reopen
admits no replacement dispatch. Caller deadlines bound observation; they do not
qualify termination of an uncooperative session operation.

Child-retirement regressions use deterministic release barriers through the
normal completion, concurrent cancel, navigator close, repeated close, stop-all,
and shutdown paths. They assert that cancellation of a caller does not abandon
its owned release, a panic after the caller is aborted still publishes a
recovery-required fault before completion, overlapping operations join the same retirement, no live
mutex is held during release, and no copy seals while cancellation is unsettled. Repeated close during an already
queued terminal seal preserves one ticket and one candidate without emitting a result for closed work,
and the existing completion retains the close intent. Composed retirement-fault
regressions hold an already dispatched effect across repeated close/stop, run one
VCS snapshot, and retain recovery evidence after both seal success and refusal.
A cloned old completion cannot consume a later same-attempt reseal; durable
reconstruction drops transient ownership and refuses the old completion.
Actual child-watcher fixtures drive canonical failed/recovery-required model
outcomes after submitted output, with and without a prior close; quota and
rate-limit classification stay intact, no submit harness runs, and exact release
still joins. A dispatched-provider barrier plus SQLite publication refusal covers
terminal recovery after failed checkpoint publication. Composed recovery tests retain existing candidates
through seal success/refusal, adoption, close/stop, binding replay, and repeated
release failure without losing the original canonical cause.
Failed normal release retains its exact-turn reservation and makes subsequent
close fail with recovery-required evidence. Shutdown tests join both existing
retirements and newly released active children concurrently, then cover the
bounded abort-and-join path and its retained fault. A two-child barrier proves
batch cancellation starts both releases before joining either, joins an overlapping
cancel, and preserves input-order reports despite reversed completion. Existing stale-turn tests
continue to exclude replacement retirement and terminal mutations. Exact-operation
replays are driven while release is blocked, after release success or refusal,
after caller cancellation, and with a replacement occupying the slot; none can
dispatch a second cancellation. Refusal to persist cancellation preparation is
also checked to leave both node state and the live canceller untouched.

Child-recovery regressions (`agent::recovery::tests` in `plakous-hosted`, the
tree's `tree::recovery` unit tests, the operator-retry tests, and one
host-loop recovery test) reproduce the stranding failure mode: a real
unknown-outcome child turn on a real runtime database, a fixture binding with
a real projected tool catalog, and a SQLite trigger that refuses the turn
waiter's own recycle reopen, so the node is left `recovery_required` and its
turn is never abandoned. Every settlement goes through the product reopen
(`reopen_spawned_child`) with the spawn's real create identity and a parked
rejoin spec. They prove that the operator's retry (JSONL `invoke_tool` of
`recover_agent`, on the host's operator port; no model catalog has it)
and a `recover_node` run each interrupt the child (law L, attempt kept, no
failure) with no additional provider dispatch, that close frees the slot, and
that a recovery operation shows the user which child it recovered. Parent-followup
regressions start with the same deterministic `OutcomeUnknown` child: plain
followup refuses, while `acknowledge_recovery` naming its exact attempt settles
recovery and admits a fresh attempt/token. Only the fresh task and its bounded
submission nudges dispatch; the unknown turn is never redispatched, and the
receipt retains the workspace-change warning. Refused recovery admits no new
attempt. Tree tests pin direct-parent/grant/attempt checks, persisted receipt
replay, stale consent, and refusal while ownership, recovery or a continuation
is pending. A lease another
session holds is refused `recovery_lease_held` exactly, by the operator's retry
after a short injected lease wait, with the child not held between its polls,
and by a run at once. Hub unit tests prove that a recovery claim excludes a
second run and a recycling child, that a run tracked on a closed hub is
aborted, and that shutdown aborts a run that overruns its allowance and still
returns. A refused tree publish changes nothing and its retry still interrupts
the child; a refusal records nothing on the node. Mail for an owed child
before its `recover_node` starts nothing and stays durable; an owed child whose
exact open restores its turn has it cancelled and is marked `owner_lost`, never
rejoined. Recovery racing the
turn waiter settles the attempt once, and a real turn waiter whose recycle is
waiting out a held lease makes the operator's retry answer `recovery_owner_live`
and the pass skip the child at once, without either waiting behind it. A
terminal refusal lets `close_agent` settle the attempt with the refusal's code
and end the node. An interrupted adopt behind an unknown
outcome settles its session first and then closes to a free slot. Startup
uncertainty is settled by replaying the spawn's create: the session is bound
and the child interrupted with its attempt open, never `completed`. Reopen errors map to exact
`recovery_*` codes from their typed kind, a live owner is never reopened
under, and a leaf caller is refused before any reopen because the tool is not
compiled for it.
The unit tests pin the settlement kinds, first-settlement-wins, attempt
fencing, close authority (`grant_denied` for a caller that does not manage the
node), that a failed copy removal is not a session recovery, that a
terminal refusal for a stale attempt is ignored, and that a
`recovery_required` node holds its tree node slot until it is closed. A
`plakous-agent` unit test pins that a turn result handle returns a published
terminal failure even when both of its channels close before the waiter reads
them.

### 1.1 The public compile contracts

`check_public_compile.py` is not a nextest target — it drives `cargo` itself,
because it must compile code that lives outside the workspace and must assert
that some code fails to compile.

`just dev-tools install` explicitly prefetches dependencies for all six locked
external fixtures with `check_public_compile.py --fetch` (`cargo fetch --locked`
per manifest), after installing the pinned tools. Prefetch never compiles a
fixture or rewrites its lockfile, is repeatable, and fails installation if any
fetch fails. Ordinary prerequisite checks and compile-contract gates remain
locked and offline; they never fetch dependencies implicitly.

Positive pins compile five external consumer fixtures. Four cover the supported
roots: `tests/fixtures/agent-consumer` (`plakous-agent`),
`tests/fixtures/facade-consumer` (`plakous`), `tests/fixtures/model-consumer`
(`plakous-model`), and `tests/fixtures/tools-consumer` (`plakous-tools`). The
fifth is `tests/fixtures/js-embedder-consumer`, the JavaScript-only workflow
that must carry no `[patch]` section. The first two are excluded from the
workspace precisely so they resolve as genuine downstream crates. These
fixtures are the inventory: a direct-package symbol outside them is nameable,
not supported.

Negative pins compile 28 external binaries under
`tests/fixtures/authority-negative` and require each one to fail with the exact
expected error code, message, and line. One locked, offline `cargo check
--keep-going --message-format=json` selects every requested negative binary;
each diagnostic must belong to that binary's Cargo target and still match its
pinned primary source span. A missing diagnostic (including an unexpectedly
compiling binary) fails the batch even when another binary fails as expected.
They cover the tool runtime and
scheduler (from `plakous-tools`, `plakous::tools`, and `plakous-agent`), the
internal process-dispatch seam, runtime checkpoints and codecs, the session
store, the durable session runtime and driver endpoint, opaque invocation and
turn-result construction, credential byte exposure, family transports, Standard
integration authority, family-artifact sealing, the raw Anthropic transport,
the OpenRouter model lane, xAI Platform authority, the local composition
session, and tree custody liveness, whose external implementation must fail the
sealed `CustodyLiveness` bound with E0277 so no crate outside the tree can mint
a `HolderGone`. The hosted construction pins provide every required factory method
but must fail the sealed `RuntimeFactory` bound with E0277, and must fail direct
`RegisteredRuntimeAuthority(())` construction with E0423. Their dependency
probe first proves the hosted types remain nameable without test support;
unrelated dependency, import, or type errors cannot satisfy the negative pins.
A separate documentation pin runs the curated `plakous-agent` and
`plakous` doctests, which include compile-fail assertions for withdrawn
prototypes.

Identical positive compile/build and lifecycle commands for one fixture or
example feature set share the stronger `cargo run`; different fixtures and
feature sets never merge. All inventory and source-content assertions remain.
`--positive-only` and `--negative-only` select a half; repeatable `--only`
fragments select pins before batching. `--archive-dir PATH` retains one complete
log per pin and the negative batch log for an evidence archive. Direct nameability of
an implementation crate is never itself a support failure. The hosted pins
enforce its explicitly closed construction boundary, not a naming ban or a
support pledge for the published-internal crate.

### 1.2 What is not gated, and why

These are real procedures with real evidence. They are kept off the daily gate
because they are slow, paid, host-specific, or measurement rather than
correctness.

| Not gated | Run it when |
| --- | --- |
| `cargo deny --config .config/deny.toml check advisories bans licenses sources` (`just dev-tools deny`) | Dependencies change. It needs advisory acquisition, so it is not offline |
| `cargo check --workspace --all-features --all-targets --locked` as an MSRV probe | The MSRV is the toolchain pin, so the ordinary build already is the MSRV build |
| `python3 tools/perf/check_prod_loc.py` (`just perf density`) | A density question is live. It always exits 0 |
| `bash tools/perf/perf-smoke.sh --check` (`just perf smoke`) | A performance question is live. Six offline product measurements and four worker timings against this machine's seed row (§2.7); informational |
| `cargo bench` for `perf_core`, `perf_project`, `perf_store`, and `tools/perf/perf_build.py` | A performance claim needs numbers. There is no portable numeric baseline; every claim carries its emitted host, feature, and toolchain metadata |
| `cargo nextest run -p plakous-offline-crash-harness` | Process-crash matrices are wanted on demand. Unix only |
| `just prompts native-compare` | Comparing plakd's prompts against installed vendor CLIs using isolated synthetic credentials and local mock responses |
| `eval/` Harbor two-rung bakeoff (Rung A weekly plumbing + TB 2.1 A/B; Rung B one-task TB 4.0 wire; `just harbor smoke` hard TB 4.0 slice) | Harness-comparison against vendor CLIs. Opt-in, paid, never a qualification row. `just harbor test` runs its adapter tests. See `eval/README.md` |
| Live scenarios (§3.2); the pre-release bar (§3.4) | A claim needs live evidence. Paid and opt-in. The bar is not a gate tier |
| `just live census` (`tools/dev/api_model_census.py`) | Before an install, to diff the bundled API model tables against the providers' lists and docs. Opt-in, networked, never a gate tier. See spec 01 §6.3 |

Local verification is the authoritative gate for every layer above; chapter 06
§8 owns the repository-settings rule.

### 1.3 Source-size reporting

`just perf density` runs `tools/perf/check_prod_loc.py` as an informational
source-size report outside every gate tier. There is no historical growth baseline,
strict density mode, or justification-file requirement.

`check_prod_loc.py` derives its scopes from each package's `layer`: `l0` is
core plus facade, `below_facade` the common, engine, protocol, provider, store, and
harness layers, `host` and `product` the packages above the facade, `test` the
repository-only packages, and `vendor` the reviewed upstream snapshot — the last
two outside the production total, reported separately, with `library` (`l0` + `below_facade`) kept for continuity. `--json` emits the path-rule responsibility
inventory, every source file with its LOC, and the reconciliation between the
subtotals.

The metric counts physical lines, including comments and blanks. It excludes
balanced inline `mod tests` bodies and named inline modules explicitly gated by
`#[cfg(test)]`, together with their attributes; production after a test module
still counts. Registered external test sources and test-file conventions are
excluded separately. Other conditional code (including platform branches,
`test-support`, and test-only instrumentation outside these modules) remains
conservatively counted. This is a source-size metric, not compiled reachability.
The gated ratchet on long-function and wide-signature allows and on files over
1500 non-test lines is separate: chapter 06 §4 owns it.

## 2. Offline test conventions

Account-reference tests cover the closed seven-vertical classification,
canonical alias spelling, reserved/unsafe names, and byte-identical native/API
profile ids and prompt renders for a bare vertical and its named reference.
Behavioral parity is not evidence of credential readiness or admission.

Named-key tests exercise all four API shelves, independent siblings and generic
secret separation. Spawned CLI commands run with a synthetic default environment
key and an unsafe default file to prove named set/status/clear never consult
either. Descriptor-read race tests replace an entry with another inode or a
symlink to the original inode, and change permissions/link counts between stat
and open; every case refuses before secret bytes are read.

Account-admission regressions require typed `unknown_account` on run/agent
selection and resumed records, while preserving known aliases and unbound
empty shells. A v12 named-API import with a synthetic default key present in
an isolated home must refuse before product-store publication, not construct
a default credential source. Account/target mismatches refuse both import and
resume before credential lookup. Product source tests use identical synthetic
keys in sibling slots to distinguish source authority from key identity, then
remove one slot and require it to become unavailable without affecting its sibling.
Mutants accepting unknown routes or bypassing
session account admission must fail committed assertions. Read-only account
labels remain distinct from admission.
Explicit rebind tests separately preserve its configured-root remedy for an
unresolvable source: valid policy yields the fallback notice and strips replay,
while invalid policy refuses without choosing a root or changing replay.

Named-native storage tests cover all three subscription verticals: unchanged
default paths/tokens, no creation on absent reads/refresh, creating-attempt
cleanup, preservation of a later receipt or recreated directory, refusal of
default/live/busy removal, and same-process login/refresh record commits while
compatibility is held. A nonblocking probe proves that the nested credential
directory is not the compatibility-lock parent before committing a record.
An occupied synthetic Codex callback pool must return typed Busy without
creating a credential or selecting an ephemeral fallback; tests never bind the
production callback ports.
Codex online/offline constructors reject the same record under a different slot
token. Native source-scope tests preserve default authority bytes while requiring
sibling names to differ without changing authenticated upstream identity.
Inventory tests prove absent reads create nothing and verticals remain separate.
Sibling refresh claims coexist under distinct compatibility locks; quarantining
one claim cannot quarantine, renew or disclose another slot's credential.
Named credential refusal rendering keeps the full-reference detail and the
existing Unspecified remedy; no frontend renders a default-slot login remedy.
Native freshness tests retain the recorded slot, refuse vertical disagreement,
and keep full-reference human detail without expanding structured remedy values.
Named Busy/429 tests discard previous quotas, plans and email, while default
last-Ready retention keeps its existing regressions.
Default API binding/configuration and the retained byte-exact v12 writer fixture
are pinned to pre-account evidence. The current writer uses v13; tests admit v13
and v12 and refuse v11 and older; seven-vertical tests pair default and named profile/prompt behavior
and capability decisions. Usage tests preserve provider email detail without
copying its reading to a sibling or default row.

Repository tooling tests live in `tests/tooling/`; opt-in live scenarios and
their status page live in `tests/live/`. Build helpers
and their authenticated inputs live in `tools/build/`, repository policy in
`tools/gate/`, and measurement helpers in `tools/perf/`. Each just module
lives beside the scripts it runs: `tools/musl/`, `tools/perf/`,
`tools/prompts/`, `tools/dev/`, `tests/live/live.just` and
`eval/harbor.just`.

Doc comments are gated on the same footing as tests: the `== rustdoc ==` step
runs `cargo doc --workspace --no-deps --all-features --locked`, then
`cargo doc -p <package> --no-deps --locked` separately for each supported
package, all under `RUSTDOCFLAGS="-D warnings"`. A stale intra-doc link fails
the gate even when only the package's default resolution exposes it.

**Compiled review windows never decide an offline test.** A subscription
profile expires 90 days after its review (spec 01 §7.1), so a test that checks
a compiled pin against the wall clock would turn red on that date. Codex and
the facade inject a clock derived from the compiled review dates (the hidden
`test-support` seams). The Grok and Claude native-auth managers read one
crate-private subscription clock: the system clock in supported builds, and
in their own unit tests and `test-clock` builds a clock that starts one minute
after that crate's compiled review and advances with elapsed monotonic time.
The facade enables `plakous-xai/test-clock` and `plakous-anthropic/test-clock`
only as dev-dependencies. The feature checker proves that neither provider's
default build enables its own `test-clock` and that no `plakd` build (default,
`maintainer`, `test-support`, or `test-support` with `maintainer`) resolves either
one; an offline test build that unifies the facade's dev-dependencies does.
SuperGrok id-token expiry is checked against that clock, not the JWT
library's wall clock. Product commands inject a fixed clock (spec 02 §8.6)
and API surfaces carry no calendar expiry. Facade fixtures that start real
provider managers derive consent and their facade clock from that provider's own review, not another route's newer review:
otherwise independently renewing OpenAI future-dates Claude or xAI consent
against those managers' deterministic clocks.
The real gateway service fixture selects those review-relative clocks for
its subscription admin operations with `PLAK_TEST_GATEWAY_REVIEW_CLOCK=1`
in the test-support process; provider replies are injected before any upstream
dial.

**A fixture process never outlives its test by more than its bound.** A test
that dies leaves its spawned processes behind: its temporary directory goes,
and a sentinel file a fixture polls for never appears. So every shell loop a
test spawns compares a pass count with a literal bound, sized to about ten
minutes (`n=0; while [ ! -f finish ] && [ -d "$PWD" ] && [ $((n += 1)) -le
60000 ]; do sleep 0.01; done`), and polls in its fixture directory so it also
stops once that directory is gone; a loop reading its input ends at end of
file; and a fixture's single sleep lasts at most 600 s. `check_workspace.py`
checks every Rust string literal, test modules included, with comma-separated
literals (argv arrays, `concat!`) joined, and refuses: a `while` or `until`
loop that neither steps a counter and compares it with a literal bound (in its
condition, or in an exit guard in its body) nor reads to end of file from an
input that has one (`tail -f` and `yes` do not); a `for ((...))` loop without
a literal comparison; and a `sleep` over 600 s. `do` and `done` count only in
command position, so prose is not a loop; shell text a test only highlights is
exempt. Load probes run at most `nproc / 2` hogs, each under `timeout` and
`nice -n 19`, killed by a trap when the probe exits.

### 2.1 Placement

Protocol-4 host row attention/counts and launcher fencing are hosted tests;
static UI validation and atomic build snapshots are product unit tests.
Directory stop tests distinguish resident actors from lease-only dormant
targets with an unavailable runner: cancellation and child-wake suppression
must be durable before the pending answer, repeat preserves the intent and
revision, and recovery cannot dispatch the stopped turn. A separately owned
probe already in flight cannot publish ready over newer stop debt; it must
perform a fresh reconciliation before admitting work.
The paused-probe regression also acknowledges an existing cancellation during
that fresh pass: acknowledgements must not dirty it again or prevent readiness.
A failed settlement always requires a fresh sweep, including on a Ready row;
the failing-owner-floor regression exercises the same recorded stop twice.
The dormant-stop regression holds the root head during both a new and a repeated
stop: the new cancellation refuses ownership without recording a stop, while
the already-recorded cancellation remains pending without changing its revision.
Review cases also require refusal after a later accepted submission, on a Ready
runner with a held head, and for a non-ownership open failure. A post-root-commit,
pre-descendant-save injected failure cut followed by a head-held retry must repair
descendant-stop rows and invalidate a paused in-flight probe before a fresh pass
can publish Ready. A failed Held-path settlement must fence before releasing its
lease; a store-locked release and a paused skipping pass pin that ordering. A
resident runtime with a recorded Stopped intent remains open across reconnect:
its ownership must not block Ready for unrelated sessions. The removed held-debt
guard is a passing no-op-equivalence check, not a mutant kill. A resident-runtime
and a branch-hold variant release their holder after the pass publishes Ready:
the previously queued bounded-backoff retry must settle the stop exactly once
without an explicit refresh, reconnect, command or restart. Repeated observations
of the same pending debt cannot queue duplicate retries or reset the delay.
Settlement clears deferred remote connection requirements, but a serving host
retains its intrinsic local-daemon requirement after the stop retry state clears.
A resident holder remains open for at least 20 seconds of paused Tokio time while Ready and non-read
AdmittedLink dispatch remain continuously available; attempt counts pin the
1/2/5/15-second ramp without full sweeps. Its release settles once and disarms.
A later accepted submission during the delay suppresses the old stop after lease
acquisition. Registry laws reject stale clearing in both the Ready-to-blocked
fence window and an already-blocked fresh pass. An attempt paused at lease
acquisition cannot overwrite a newer pending set or its retained context, and
the newer set keeps its timer armed after the old attempt completes. The review driver
reseeds the head-free repair, fresh descendant-debt fence and release-ordering
decisions; build failures and empty selections are not mutant kills.
Neither an attachment nor a paid continuation is created by a stop.
The fixture stops a session with no node attachment, checks durable
cancellation at the stop receipt and observes the changed directory row on a
watch. The live child counts are a projection, not part of the stop receipt.

Host serving is not a runner-readiness barrier. Daemon recovery fixtures use
the product's local `Ready` verdict, published after ownership sweep, answered-key
recovery, recorded stops and foreground cleanup, before inspecting daemon state
or asserting an empty journal. The pre-hello EOF fixture also waits for that
verdict before shutting down the host, so shutdown cannot interrupt the
reconciliation being tested. Root- and child-stop crash fixtures cross that barrier
before counting terminals: physical death can precede the durable terminal
receipt. Uncut readoption and stop pilots cross the same barrier before shutting
down and reading their complete commit traces; the stop
pilot still requires process death after settlement. Failure fixtures require the
current host's typed blocked verdict as well as the diagnostic, so a previous
host's log cannot satisfy a restarted host's failure proof. Empty daemons may
have no owner-floor file until their first ownership transfer.
The predecessor-stop journal proof waits for readiness before its fixture takes
the line lease; it must not race the host's legitimate lease-only sweep. These
regressions run with the workspace all-feature artifact set used by durability,
not only the product package's separate feature resolution.

Runner authority shapes in the strict sweep own explicit process/link cuts,
rather than pretending their clocks are
SQLite commit ordinals. `runner_wake_settled_foreground` freezes runner awake
time while the driver settles, then checks keyed cleanup before a new effect.
The settled fixtures hold a real answered/Acked start at the tool-link seam,
pause the daemon, and inject loss of that answer only after the explicit
pause/release rendezvous. The 1200 ms read/effect windows stay unchanged.
Before resuming, settled fixtures also require the eager reopen's explicit
`RecoveryIncomplete` observation: an idle terminal alone does not prove that
reopen ended, and a still-live reopen may fold outcome evidence after reconnect.
No equal-duration sleep orders those phases. Feed assertions distinguish the
original operation's one physical outcome from its one later authority fold:
`OutcomeUnknown` followed by `Finalized` is not a second physical settlement.
Before/after cleanup the counts remain one outcome and zero folds; after the
subsequent reopen both counts are exactly one. This also avoids racing an
acknowledged start's later read: that read is correctly placement-held, so
waiting for its terminal before allowing reconnect would deadlock the fixture.
`runner_wake_settled_foreground_after_delayed_reconnect` resumes 2500 ms after
the terminal. Following effects have a 30-second observation deadline, fresh
request ids and at most four rejected drafts, spaced by the documented
1/2/5/15-second reconnect backoff. Only a runner read that did not reconnect,
or an explicitly not-ready runner placement, may be retried; lease-held,
foreign-owner and local-reconciliation refusals fail at once. The zero-delay
case retains two immediate attempts. An answer without its effect fails.
`runner_reopen_waits_for_its_readoption` holds the real lease-only sweep, sends
one prompt, and releases only after the admission waiter is observed: that
single prompt must run its effect after cleanup without a rejected draft.
The process-local gate also pins the two-second bound, separate database scopes
and a sweep skipping an in-progress root open. The real-host prompt regression
uses independently constructed workflow handles over the same database.
`runner_wake_live_foreground` reconnects inside the effect window and proves
natural completion. `runner_lease_lapse_reports_foreground` advances awake
time past the lease without a wall-clock wait. `runner_new_owner_foreground`
cuts newer-owner transfer and receipt recovery. No foreground case may insert
a job-terminal row or revise an already-settled call.
`runner_deferred_pending_stop` uses a separately paired runner home, stops its
transport while its host remains alive, durably cancels without an admitting
open, and cuts the later first-bind lease/transfer/terminals/before-stop/
after-stop phases. Restart and repeated stop must preserve the original
intent and never redispatch the promoted job. `RunnerOptions::lease_clock`
is an explicit, nondecreasing suspend-excluding monotonic test/embedding port;
absent, production uses the platform `Instant`. Its controlled-clock proof
checks that frozen awake time preserves foreground, advancing it stops
foreground but preserves promoted jobs, and report cuts before receipt
acknowledgment retain exactly the same report. No wire or binding input changes.

Placement-hold regression cuts also retire a child live entry without Stop
during a corrective nudge and during transient instruction admission. Joining
the turn waiter must leave the attempt open, without result mail. Non-runnable
resident cuts before admission and after instruction preparation instead require
retirement, for both closed and reconciliation-required attempts. Hosted wake
binding cuts close placement after its immutable receipt is prepared: release
admits the unread batch once under a fresh identity, while Stop leaves no
unresolved receipt. Root loss-resume tests distinguish pending production
from an already prepared delivery, and the run-until-idle test releases at
the Deferred cut without replacing a completed result by Interrupted.
A local-home `RoutedVcs` fixture drops the remote child's route and checks the
stored seal stage at dispatch, including mixed local/remote ticket indices.
Live-progress label and attention-contention tests distinguish held from
refused placement and cached placement from stale login attention.

Test code has one home per kind, and production behavior is never moved into
test code to change density accounting.

The strict authority registry also includes the seven `runner_hold_*` shapes
below. They run against a real serving product and a separately paired daemon
through a framed fixture bridge, not SQLite-ordinal substitutions. The daemon's
supplied awake clock is frozen before startup; SIGSTOP alone is not suspend
evidence. Every pause has Drop cleanup and every process loop has a literal
bound. The strict default selection includes all seven shapes.
These strict shapes fail with a short-`PLAK_TEST_TMPDIR` diagnostic if their
Unix socket path is too long; they cannot silently pass by skipping the fixture.
The pre-send cuts require NotSent while the daemon is still paused, before
rebind. Their five-second liveness guard is below a separately configured
thirty-second read window: it tests admission ordering, not a 300 ms scheduling
promise, and waiting under client locks still fails that cut.
Scripted product bindings attach the real host account budget through
`SessionRuntimeBinding::with_request_slots`; merely writing a one-slot config
without binding that shared budget is not isolation evidence. The queued-nested
fixture records actual executor entry and correlated delivery, requiring neither
before release and exactly one after release. Refusal cuts before first dispatch
and after preparation also require zero paid model requests.

| Shape | Authority cut and oracle |
| --- | --- |
| `runner_hold_poll` | An Acked foreground start, a claimed observation before bridge forwarding, link loss across the read window and separately the effect window, then reconciliation held before Ready. Natural completion, no unknown/cleanup stop/promotion, unchanged paid count while held, and equal normalized operation-id/state sets and terminal/effect counts across the two timings. A second loss after the barrier but before publication must return the real client's `link_lost` verdict, not stale Ready. |
| `runner_hold_live_key` | Withhold an actual runner result before Ack; retain the pending model key past its independent effect window. Rebind must replay that original key/digest with its original age, execute once and admit no paid triage. |
| `runner_hold_not_sent` | Stop after the retained placement's availability read but before the client call, then disconnect before transport enqueue. Require typed NotSent, no effect before Ready, and exactly one eventual start, without replaying a composite prefix. |
| `runner_hold_stop` | Stop a held observation with a queued mutating suffix while the daemon stays absent. Require bounded terminal publication and no suffix, extra provider request or continuation after release. |
| `runner_hold_host_loss` | Kill only the host while held; serve the reopened directory before the runner returns, then cross unpaid recovery. No in-place hold restoration, one lost-turn terminal and no continuation consent from Ready. |
| `runner_hold_identity` | Replace the saved identity while an unsent call waits, and separately restart the same saved runner with a sent key outstanding. Old waiters close rather than strand; fixed unknown closes before Ready and is never a fresh send. |
| `runner_hold_isolation` | With request slots set to one, hold a remote root while another root of that same account on the local runner completes. Keep the first root busy/unsettled, then release it once. The product has no cross-runner child spawn field; child/tree isolation is separately exercised by orchestration fixtures, not claimed by this two-root proof. |

The offline script's test-only `runner_windows` object independently names
`read_ms`, `effect_ms`, `ping_ms` and `dead_ms`. The same object supplies local,
remote and scripted-child dials; no shared read/effect-window shortcut remains.
Production builds keep their ordinary windows. `runner_hold_cuts` names an
owner-only fixture directory: pre-send entry/release sits outside client locks;
pre-Ready entry/release and result record the actual publication verdict, and a
link-loss marker observes that retained client's connection. The framed bridge
records only request identity/class/digest/replay/age and verb, never payloads,
and can stop forwarding a poll or withhold an answered key before Ack. These
are test hooks, not product configuration, wire fields or persisted hold state.
There is no optional hold cap in the product to shorten in these fixtures.

Live paired-machine acceptance remains separate: real suspend, awake lease
lapse, matching source/binary identities, host restart and external-client review
are not inferred from these controlled process cuts.

Protocol-5 contracts pin closed request/answer/progress JSON and the generated
schema together. Placement tests distinguish committed UUID replay from fresh
admission: changed profiles/defaults, renamed aliases, missing paths and offline
runners cannot invalidate stored-fact replay. Shared roster tests alternate two
saved profile selections through one process-wide source and retain the None
home-default path. Tool-row tests pin unchanged stored/export bytes and JSONL v1,
and require host operation preservation through snapshot, patch and stream
decode, including verbs the classifier does not currently emit. Client rendering
uses the received operation rather than the display name.

Draft-id tests distinguish mailbox acceptance, pending, durable acceptance,
definite refusal/drop, conflict and capacity. Reconnect, two simultaneous devices,
disconnect before delivery, session switching and host restart are distinct
cases. Only durable acceptance survives a client disconnect; restart must not
invent a durable verdict. Picker tests require bounded request-scoped progress
before one final answer and no runtime creation. Archived attachment tests serve
persisted facts with zero runtime opens and refuse execution even when the
client ignores the archived UI state. Lifecycle tests exercise commit/crash cuts,
idle-release and lease/dependency fences, concurrent metadata preservation and
retryable deletion cleanup, not only setter round trips. Mutation evidence must
kill the corresponding rule-breaking variants; a passing happy path alone does
not prove admission, quiescence or unchanged persistence boundaries.

| Kind | Home |
| --- | --- |
| Small private-invariant tests | Inline `#[cfg(test)]` in the crate |
| Public component contracts | That crate's `tests/` directory |
| Reusable deterministic doubles and fixtures | `tests/support/plakous-test-support` |
| Cross-crate, crash, product, protocol, and performance procedures | Workspace consumer packages under `tests/` |

The `tests/` tree holds the consumer packages:

| Path | Package | Role |
| --- | --- | --- |
| `tests/support/plakous-test-support` | `plakous-test-support` | Scripted models and tools, scenario and barrier helpers, conformance drivers. Reachable only through the public `plakous-model` and `plakous-tools` contracts; never reexported by the facade |
| `tests/harnesses/offline-crash` | `plakous-offline-crash-harness` | Public-facade SQLite `SIGKILL`/reopen matrices, and the declarative offline model script (section 2.6). Depends on `plakous` with `sessions-sqlite` and exports narrow loopback builders to `plakd` as a dev-dependency, and as a `test-support`-only optional dependency. Its finalization-debt fixture uses `plakous-agent` and `plakous-store` to commit a real debt-only checkpoint for hosted recovery tests without exposing checkpoint internals to the hosted crate |
| `tests/fixtures/{agent,facade,model,tools}-consumer` | not workspace members for the first two | The supported external workflow inventories driven by `check_public_compile.py` |
| `tests/fixtures/authority-negative` | — | The 28 compile-fail pins |
| `tests/performance/baselines` | — | The perf-smoke seed rows and the allocation probe |
| `tests/live` | — | The live drivers, the status page, and the pre-release bar (§3.2, §3.4) |

The annotated reference plakd home lives in `docs/reference-config/`. Scenario
homes live in `tests/fixtures/config-scenarios/` and are loaded by an offline
`plakd` test so they cannot rot (section 4). Built-in provider homes live in
`bin/plakd/presets/`.

Every path above is repository-only. Test-support, crash-harness, and fixture
packages are never publication candidates.

The offline crash harness production closure is public-facade-only: the
`sessions-sqlite` feature, a loopback `LanguageModel` and a loopback mutating
`Tool`, so it cannot reach a live or paid provider. Parent and child exchange
one versioned JSON-lines command stream, and the child fsyncs separate
provider and effect evidence before it reports each cut boundary. After
`SIGKILL` and reopen through `AgentSessions`, a cut accepts only the exact old
state, the exact new state, or a typed recovery-required classification, and
reopen or recovery never appends provider or effect dispatch evidence.
Recovery, context status, compaction, fork and rebind all return the same
recovery authority. The public facade has no hook at the private checkpoint
transition or compaction publication, so the provider-terminal to checkpoint
interval is labeled split evidence, closed by `OpenedSession::release`
returning its durable locator; the harness claims no direct injection at a
private publication fence and no process cuts of composition. plakd's own
product-journal and host `SIGKILL` matrices are private plakd tests.

### Model fuzz targets

The standalone nested workspace `crates/core/model/fuzz/` keeps fuzz-only
dependencies out of the production workspace and lockfile. Its targets cover
response assembly, semantic JSON canonical idempotence and serde reconstruction,
and portable-schema compilation, reconstruction and instance validation.
From that directory, use a pinned `cargo-fuzz` installation and an explicit
run bound, for example:

```sh
cargo fuzz run model_response_assembler -- -runs=10000
cargo fuzz run semantic_json -- -runs=10000
cargo fuzz run portable_schema -- -runs=10000
```

The local `.cargo/config.toml` puts build output under the repository's
`target/plakous-model-fuzz/`. These opt-in campaigns are not ordinary gate
evidence.

### 2.2 nextest profiles

`.config/nextest.toml` defines the store directory, test groups and lane
profiles. Nextest layers each lane over `default` and inherits its settings,
groups and overrides. A lane supplies membership, scheduling and timeout.

| Profile | Membership | Use |
| --- | --- | --- |
| `default` | Everything nextest selects | Ad-hoc runs |
| `offline` | Everything not assigned to serial, durability or install | The parallel lane: `tests-parallel`, `just fast`, `just steps tests-parallel` |
| `serial` | Wall-clock budget tests (§2.3) | The serial lane: `tests-serial`, `just steps tests-serial` |
| `durability` | Core crash matrices and the SQLite SIGKILL campaign (§2.3) | Batch core durability and explicit `tests-durability` |
| `install` | Additional upgrade, runner, restart, residency, retirement and timing families, and the two exclusive indexed crash-sweep campaigns (§2.3) | Linux install, Mac install checks, full, and explicit `tests-install` |

The three `ledger_retirement_5000_*` tests are ignored long-turn evidence, not
routine serial work. `just soak` builds the real workers and selects those
ignored tests and the four full indexed retirement matrices, with one test
thread and the normal isolated test environment.
It runs the 100-cell × 50-leaf fixture against memory and SQLite, plus 5,000
direct batch calls, at production retirement thresholds. `just full` runs the
same long evidence in the serial and durability groups. Batch, install,
fast and explicit test groups never opt into ignored tests. To qualify long-turn evidence
on demand, run:

```sh
just soak
```

The parallel lane runs the low-threshold closed-cell/reopen fixtures for
both stores and the small direct-batch fixture. The nested refusal/recovery
fixture remains in serial.
They still exercise retirement, charged headroom refusal and the authenticated
counter without paying for 5,000 calls on every integration gate. Ignoring the
large fixtures does not change the four lanes' membership partition; only
the full tier and the soak recipe pass `--run-ignored only` with the narrow
`test(ledger_retirement_5000_)` serial filter and
`test(soak_ledger_retirement_)` durability filter. Other ignored measurements stay
opt-in.

Install `durability_ledger_retirement_*` campaigns derive a deterministic set
cover from twin-pilot publication metadata: every SQL signature, changed
operation-kind/state class, and retained anchor kind/state and side of P must
be covered. Anchor classes at authenticated counter increases are covered
separately; selected retirement cuts include their neighbouring runtime
publications. Each store's first and last publication is included. A campaign
refuses a selection over 40 cuts instead of silently dropping coverage, and
runs its selected cuts on four isolated workers. The ignored
`soak_ledger_retirement_*` twins retain every indexed cut on one worker. Pilots,
early relaunches and the bounded campaigns run together in the install profile;
the existing small anchor/refusal/closure tests remain routine.

Every profile is hermetic: no test in the workspace reaches a network or reads a
credential. Batch runs the parallel, durability and serial lanes; full runs all
four lanes. Mac batch filters the three core lanes to platform-sensitive tests.
An `-E` filterset on the command line narrows a
profile's membership (nextest intersects them), so
`--profile offline -E 'rdeps(=<pkg>)'` runs a package's parallel-lane tests and
never its serial ones. `fail-fast` is off. The workspace slow timeout is 60
seconds with termination after the second period; serial, durability and
install raise theirs to 240 seconds.

Each lane profile writes a JUnit report to `target/nextest/<profile>/junit.xml`
(the store directory, gitignored), overwritten by the next run of that
profile, including the `offline` and `serial` runs of `just dev coverage`.
`just dev test-timing [LANE...]` (`tools/perf/nextest_timing.py`) reads the
reports present and prints, per lane, the test count, wall time, summed test
time, effective parallelism, failed, flaky and retried tests, the longest
test and the test span (first test start to last finish, from per-test
timestamps; without them, the longest test as a lower bound), then the
slowest tests and the per-binary and per-package totals. It is informational:
no gate reads it.

`just dev weighted-lanes [--serial-threads N] [--durability-threads N] [LANE...]`
(`tools/dev/weighted-lanes.py`) trials a multi-thread layout for the two
wall-clock lanes without changing their gate settings. The ordinary durability
lane is already weighted; the trial varies pool sizes and serial concurrency.
On every run it writes
`<target>/nextest-weighted.toml`: `.config/nextest.toml` plus `weighted-serial`
and `weighted-durability` profiles with the `serial` and `durability`
membership and settings, 4 and at most 16 threads by default.
Restart reservations scale to one quarter rounded up.
The trial reuses the
gate's scaling implementation and caps its width by the host. It refuses a
weight filter that matches no test, builds the workers as the gate does, runs
the lanes through `--config-file` in a scratch directory of its own (the
per-checkout one plus `-w`), so it leaves a gate's files in the same checkout
alone, and ends with `test-timing` over its profiles. Its load is not isolated:
beside a gate on one machine, timing-sensitive tests fail in both, so it runs
when no gate does or with lower thread counts. No gate tier runs it; a failing
lane exits non-zero.

Standalone runner fixture requirements are:

| Entry point | Fixture-path contract |
| --- | --- |
| `dev crash-sweep`, `dev weighted-lanes` | All four paths from the runner's workspace Cargo artifact capture |
| `dev coverage` | All four caller-supplied executable paths, validated before coverage starts |
| `just nextest` | Caller supplies the four paths for full fixture coverage |
| `dev jj-compat` (`jj-tools.py`) | The `test(jj)` selection exercises repository/JJ and doctor probes, not cell, MCP or product-crash fixtures |
| `generate_binding_fixture.py`, `fixtures/current_bindings.py`, `fixtures/bootstrap_bindings.py` | Binding/export generators use explicit synthetic, never-invoked worker artifacts, not test-build executables |
| `dev test-timing`, `dev crash-compare` (`compare_crash_sweeps.py`) | Read existing reports only; no executable fixtures |
| Reload rehearsal | Execute staged product/sidecar snapshots, not Rust test harnesses; no MCP-server or crash-harness fixture variables |
| Performance smoke and cell timings | Execute the product and matching copied workers, or workers by explicit paths; no MCP-server or crash-harness fixtures |
| Performance checks, allocations and build probes | Run pure core/project/store benchmarks or compile-only measurements; no worker/MCP/product-crash fixtures |
| Live drivers | Execute built product/sidecar images; MCP OAuth uses its own HTTP server, not `mcp-fake-server` |
| Harbor | Executes uploaded product and matching workers with explicit remote `PLAK_JS_HOST`/`PLAK_PY_HOST`; no Rust fixture suite |
| Musl native/container gates | Delegate to `dev-check.sh`, which resolves fixtures inside the native/container build rather than passing host paths into a container |
| Prompt render/diff/pin recipes | Run render-only unit tests with synthetic bindings, not worker/MCP/product-crash fixtures |

The `default` profile sets `leak-timeout = { period = "5s", result = "fail" }`;
the lane profiles inherit it.
After a test process exits, nextest waits up to five seconds to observe EOF on
its captured stdout and stderr, and fails if EOF has not been observed. A
shorter window false-failed in-process tests whenever the runner stalled under
gate load.
That result does not prove a handle physically remains open or count child
processes. Tests that own processes must still prove their own bounded, natural
or forced joins.
A prior LEAK observation under the runner's shorter passing default does not
by itself establish whether that test leaked a process.

The integration suites in `plakd`, `plakous-agent`, `plakous-tools`,
`plakous-store`, `plakous-openai` and `plakous-compose` each link one
`integration` target rooted at `tests/integration/main.rs`. The former target
files are sibling modules, so their test names have the former target name as
a module prefix. Store's
`fixture.rs` is a library-test helper, not an integration target; the nested
SQLite conformance module retains its independently inventoried tests.
Product modules gate telemetry on `test-support`, offline end-to-end tests
on `test-support`, and reload rehearsal on `maintainer`; store gates its
SQLite module on `sqlite`. Default-feature `cargo test` compiles the ungated
modules, and the all-features gate includes every module. Relative fixture
includes follow the module location; manifest-root fixture paths do not move.

### 2.3 Serial groups and the budget lane

Placement-hold admission and process-yield cuts (`placement_hold_cut_`)
run in the install lane with exclusive reservations. The cell deadline and recovery-notification cuts use
paused clocks. Offline tree cuts distinguish an unstarted seal reservation
from its published dispatch stage, assert no candidate/mail/parent settlement
while deferred, and let an unrelated sibling seal. Wake reservation/binding,
unclaimed mail, corrective decision admission, child-only attention and exact
owner-loss debt release have separate assertions; live runner sleep is not
inferred from these fixtures.

Dormant-stop recovery and real-process UI signal reload scenarios belong
to the serial lane, excluded from offline parallel execution. Their waits
are for explicit observations, log acknowledgements or process settlement.
Reload-signal probes cover both blocked runner startup and a control-only
host. Launcher timeout evidence gates mailbox reads, checks the bounded
refusal, then resumes idle, busy and process-held actors and verifies fresh
attachments still reach the same actor with its runtime lease intact.
Ticket tests cover both cancellation before a late idle quit and a committed
release winning the expiry race.

Three mechanisms keep wall-clock and resource assumptions honest, and none of
them relaxes an assertion to buy parallelism.

**Test groups** cap concurrency across the whole workspace:

- `native-auth-serial` (`max-threads = 1`) covers every `native_auth::` test and
  the `local_transport` binary. Native subscription auth tests share
  process-local fixtures. No test binds a pinned OAuth callback port (Codex
  1455/1457, xAI 56121): browser logins in tests bind an ephemeral port through
  the managers' loopback seam, a `cfg(test)` assertion refuses the pinned ports
  in both managers, the facade's unit tests switch Codex to the seam through
  `plakous-openai/test-support`, and `tests/tooling/test_pinned_callback_ports.py`
  refuses a pinned-port bind or an unguarded browser login anywhere in the tree.
- `cli-contract-serial` (`max-threads = 1`) covers `cli_contract::` in plakd's
  `integration` binary, selected by package, binary and anchored module name,
  not the other modules sharing that binary.

**The serial lane** is the `serial` profile: four test threads on Linux, one
on macOS, and a 240-second
slow timeout with termination after the second period. Its `default-filter`
lists the tests whose subject is a wall-clock measurement rather than a scheduler
outcome. The tables below group the members by kind; `.config/nextest.toml` is
the exact list.

Short real-clock checks in this lane cover executor shutdown and application
result/observation ordering, worker stdin/cancel and SIGTERM escalation, callback
writer and private-store lock deadlines, unfinished-upload closure and
detached-writer cleanup. Two cleanup-contention tests assert a two-second
mutex-admission bound; the mixed-lock probe starts with one already-exited,
unretired child so the short budget measures admission, not process-group cleanup.
The 64/128-process capacity probes and pre-spawn refusal test are serial. The
64-child running-limit cases hold the children on stdin and join them after
individual completion, rather than making concurrent termination part of the
limit assertion. A 240-second runner allowance is a hang watchdog, not a
replacement for any local assertion. Individual predicates identify these
tests; an `application::` substring would also match unrelated prefixed modules.

The steered job-await fixture uses a process that survives stdin EOF until
explicit stop. Closing stdin during termination must not race a `read` command's
natural exit and obscure the stop attribution being asserted; natural-exit
preservation remains covered separately.

A hosted quit regression expires the terminal-join deadline only after the
selected result future has resolved into the fixture's result channel, before
the host observes that ready result. Control settlement alone is not this
barrier: it can wake while terminal installation still owes result-channel
notification. The test-only forwarding task preserves the actual result and
makes both fixture receivers ready before injecting expiry. The regression
requires a clean quit and the canonical cancelled lifecycle, not lease-only
abandonment. The injected deadline models host descheduling without a wall-clock
wait, so this regression lives in `runtime::quit_tests` and runs in the parallel
lane; production selection, deadlines and serial quit budgets are unchanged.

Stop-cut matrices panic the actor, not a separate process. Even an idle actor's
join is not proof that its separate driver's graceful lease release completed:
bounded executor teardown can cut that release short. Before exact reopen, the
fixtures read the lease row without acquiring or changing authority, wait for
release when it completes, and otherwise wait out the existing lease period
plus two seconds. Child-stop and account-continue cut fixtures open their
shared runtime with a two-second test lease before spawning its worker.
The still-live test process is never treated as a dead holder.
The root-stop matrix retains its long hang watchdog across repeated cuts.
Each child-stop cut runs as a separate test under the normal per-test watchdog;
both retain all continuation, cancellation and wake assertions.

The real-binary `real_*quit_releases*` checks accept exactly the two specified
quit statuses: joined release (0) and lease-only abandonment (130), never a
signal death. The JSONL check correlates that status with `cancelled` versus
`unresolved` for the admitted turn. Both paths still require the owned child's
TERM witness and a successful next turn
after reopening exactly the same session. Host load can legitimately spend
the bounded cancel/join windows; requiring only exit 0 would assert an
unpromised scheduling deadline rather than release and recovery.

A real accepted cancellation cut leaves its provider round, evaluation and
single child finalized, its nested effect Completed, and its root Dispatched.
`execute_restored_admitted` returns on cancellation before pushing the effect
into the cell's child-operation list, so only the root terminal commit's
`finalize_turn` would finalize it. Its outcome was not consumed. Recovery must
close that root and finalize the effect with its own evidence, without replay.
The regression reopens the unedited accepted checkpoint in a fresh runtime;
a same-attempt effect whose call identity has no child must not close.

The product-revision CAS probes run serially with `PLAK_TEST_CAS_SAMPLES`
independent homes each (positive integer, default 6, two process launches per
home). `PLAK_TEST_CAS_SAMPLES=30` selects a thirty-home qualification probe.
Idle quit/reopen alternates the home-owned tree gate on and off.

Four real-JSONL-face SIGKILL cells run in the durability lane: idle before a
turn, inside the model request, inside a running tool and after the answer.
The approval-cut cell specifies the restored-decision contract (same call/tool,
no redispatch or abandonment), but remains explicitly ignored under
`JSONL-APPROVAL-RESUME` until the production reopen hang is fixed. Its durability
predicate enrolls the cell when it is enabled.
The workspace/all-features prerequisite builds the
test-support product and matching sidecars; the fixture uses `PLAK_TEST_BINARY`
from that build's Cargo artifact records, or an explicit executable override.
All five are excluded from the parallel lane. Fixture-owned hosts are reaped
on failure.

Paused-Tokio elapsed checks stay parallel, including the quiet-poll repeat-hold
tests, protocol-terminal close, transition-stage and subscription-refresh
timeouts, and child completion/start-failure waits. The terminal UTF-8
observation test waits for real process completion before pausing Tokio time,
so its return bound measures observation rather than process startup.
Host-worker join and usage
refresh drop use channel-held workers: teardown must complete before the held
work is released, with a thirty-second scheduling watchdog. Empty-output poll,
empty first-terminal await and steered Grok await run in parallel with a
120-second requested window and a thirty-second return bound. They test wakeup
versus waiting out the window, not a subsecond latency budget.

Four cleanup-contention probes run in parallel with scheduling slack rather
than short latency budgets. `contended_operation_does_not_stall_joined_proof_probe`
waits for its response while the operation lock remains held, with a
thirty-second hang watchdog; a probe blocked on that lock cannot return before
the guard is dropped. `terminal_status_without_joined_owner_is_not_durable_proof`
uses a sixty-second backend cleanup duration and a thirty-second probe bound
to detect waiting on an unfinished supervisor. The retirement checks
`promoted_short_job_is_retired_without_observer_polling` and
`promoted_monitor_exits_on_owner_drop_without_terminal_publication` use
thirty-second hang watchdogs: they prove eventual retirement without observer
polling and monitor exit after owner drop, not subsecond latency. A live
child's test teardown retries `join_all` until the service is drained or
thirty seconds pass. Each attempt still uses the fixture's cleanup window,
which contention tests shorten so a held operation returns inside the
admission bound; the outer wait is not three of those windows, because a
shared process-group scan under parallel load can miss a few hundred
milliseconds three times without the child being unreapable. Closed-stdin
completion (`cat` and a finished `printf`) uses the same thirty-second
yield: the assertion is that the process exits on EOF, not that the host
scheduled it inside one second.

A test's wall-clock wait that only guards against something never happening
(a `tokio::time::timeout`, `recv_timeout` or deadline loop whose expiry fails
the test) is a liveness bound, not a timing assertion: it is thirty seconds,
so load cannot trip it, and a hang still fails well inside the lane's
allowance. A bound that distinguishes returning promptly from waiting out a
longer grace or window is a timing assertion: it runs in the serial lane, on
a paused clock, or event-driven. A retry window (a short attempt that is
repeated) is neither and keeps its length. An absence check (nothing happens
within a window) keeps its window, and so does any wait whose result the test
inspects rather than fails on (`is_ok`, `is_err`, `if`, `match`, `while let`).
A bound in a test that pauses or advances Tokio time is virtual time and
keeps its value, and a wait in a test that also sets a product-side window
or grace at least as long keeps its value, since that window's expiry could
satisfy the same assertions.

CPU-time redaction checks also stay parallel:
they measure process CPU, not scheduler delay. Real-clock lower-bound-only
assertions (wake consumption, retry delay and progressing streams) do not fail
because the runner was descheduled and stay parallel. Pure duration-value
assertions do not measure elapsed time and need no budget lane.

| Kind | Predicates |
| --- | --- |
| Observation parity | `observation_parity_tests::` -- the whole module: the parity scenarios (plain turn with a tool call, tool error, capacity block, clear then turn, interrupt then second prompt, escape then quit, context placeholders, compaction, spawn/wait/adopt), each comparing against its golden under `bin/plakd/tests/parity/`, plus the normalization, wait-receipt, cancelled-tool terminal and JSONL terminal tool-line checks |
| Process spawn, drain and cancellation windows | `exec_command_with_zero_yield_returns_the_joined_output`, `host_process_bounds_live_output_and_enforces_timeout`, `joined_process_pty_cancellation_and_timeout_join_the_group`, `cancel_during_a_cell_running_a_real_process_command_joins_inside_the_settle_window` |
| Python worker request deadline | `a_stuck_worker_request_is_killed_and_reported_as_session_loss` — uses a one-second fixture request deadline, isolating bundle setup from parallel load without weakening the stuck-request kill and fail-closed loss assertions |
| Cleanup mutex admission | `terminate_contention_is_bounded_and_preserves_correlated_owner`, `join_all_handles_free_children_and_retains_contended_child_for_retry` — the two users of the two-second `assert_bounded` helper |
| Executor/application ordering | `executor_drop_does_not_wait_out_spawn_blocking`, `result_before_terminal_event_returns_without_drain_or_hang`, `event_before_result_is_presented_without_becoming_terminal_authority`, `observation_error_before_result_is_non_authoritative_and_does_not_hang`, `ready_result_ignores_late_observation_error_without_hanging` |
| Worker stdin and SIGTERM escalation | `nonreading_stdin_cannot_block_start_or_cancel`, `a_worker_that_ignores_sigterm_is_killed_inside_the_shutdown_bound` |
| Process observation and job-await budgets | `process_await_wait_does_not_reserve_a_blocking_worker`, `terminal_hint_drains_more_than_one_record_batch_without_waiting`, `contended_observation_does_not_block_the_async_worker` |
| Process capacity probes | `running_children_still_count_against_the_session_limit`, `sixty_fifth_running_start_is_refused_before_spawn`, `host_capacity_refuses_the_129th_start_across_services_before_spawn`, `a_refusal_raised_before_any_spawn_reaches_the_model_as_a_tool_failure` |
| Product CAS startup/reopen | `isolated_jsonl_startups_and_reopens_preserve_product_revision`, `isolated_idle_jsonl_quits_and_reopens_preserve_product_revision` |
| Crash-sweep view rebuild | `full_sqlite_view_rebuild_and_plain_reopen_equal_incremental_for_every_sweep_shape` — runs every registered sweep shape's lifecycle in sequence in one test (38 to 58 seconds alone); under parallel gate load it outran the parallel lane's 120-second allowance |
| Auth lock/callback and HTTP closure deadlines | `expired_commit_and_stalled_callback_writer_are_fail_closed_and_bounded`, `busy_execution_lock_fails_without_waiting_or_accepting`, `an_early_response_to_an_unfinished_upload_is_not_kept`, `a_post_acceptance_deadline_is_definite_on_both_wires` |
| Detached writer cleanup | `host_future_drop_is_bounded_with_a_detached_continuous_writer` |
| Auth deadlines | `cancelling_claimed_refresh_after_request_observation_restores_ready`, `local_browser_rejects_wrong_host_without_dispatch_and_returns_no_store` |
| Mail yield window | `unread_mail_yields_are_bounded_within_one_tool_batch` measures elapsed time and stays in serial |
| Host quit, compaction and session round trips | `interactive_observation_timeout_releases_gate_and_joins_without_masking_failure`, `interrupt_then_quit_publishes_the_cancelled_terminal`, `quit_during_active_turn_waits_for_cancelled_terminal_projection`, `scheduled_compaction_failure_keeps_host_worker_alive`, `product_session_facade_create_turn_shutdown_and_reopen`, `retiring_a_live_child_advances_its_session_locator`, `real_session_late_start_ack_releases_after_deadline_without_redispatch` |
| Wall-clock promptness bounds | Idle intent and process wakes, TERM grace and process exit, fetch and credential observation deadlines, quit and recovery admission, bounded user stop and jj doctor waits — each asserts a one- to five-second return rather than waiting out a longer grace or window. Parallel load can trip these real timing assertions. |
| A real race, listed not hidden | `completed_terminal_outcome_remains_authoritative_when_exit_was_requested` |
| Product durability scenarios | `offline_e2e::durability::` — quit, resume and failure-wave tests. The separate gateway and subscription custody durability modules run only in core durability |

`join_all_terminates_a_running_free_child_and_retains_the_contended_child`
runs in the parallel lane. It requires the running free child to be
terminated and joined while the contended peer is retained for a retry;
it does not assert wall-clock promptness.

The bridge log-lock case holds the lock across a one-second discovery window
after observing a refused auto-start, then requires a real Welcome. The
hanging jj probe and stalled filesystem fixtures use one- and three-second
test deadlines, respectively, and still require the correct timeout outcome
from the real subprocess path. The shipping discovery and subprocess windows
are unchanged.

`native_ready_lease_is_held_through_head_and_released_before_stream_body` runs
in the parallel lane. Its logout-start, response-head and response-body barriers
prove exclusive store-lock contention before the head, then await successful
logout while the body is still gated; there is no negative timed wait.

The generated-actor fault schedules and success permutations run with paused
Tokio time in the parallel lane. Their case and dispatch/join deadlines measure
idle scheduler progress, not host speed; dispatch polling sleeps so an absent
dispatch reaches its virtual deadline instead of yield-spinning. A negative
no-dispatch fixture pins the elapsed deadline error. Tokio inhibits automatic
time advance during blocking work, so joined blocking cleanup is not mistaken
for a lost wakeup. All joined-terminal assertions and fixed seeds remain intact.

The module predicates select whole suites. Observation-parity tests
share the host turn machinery and its wall-clock budget, so the module is the
unit rather than an arbitrary subset of its tests.

`local_browser_rejects_wrong_host_without_dispatch_and_returns_no_store` is in
the lane despite running in 0.01 seconds. Its budget is internal: it races the
loopback callback against a `tokio::time::timeout(Duration::from_secs(2), …)`
on the login completion, and `??` turns an elapsed timeout into a failure. The
fixed callback port is already serialized by the test's own gate and by
`native-auth-serial`, so the port is not the race — the two seconds are.

Reserving the whole test pool inside one parallel run is not enough on a loaded
machine: it stops other tests from running alongside, but each reservation is
then measured against whatever the machine was doing, and reservations
interleave with each other. So the gate runs these members in their own lane.

**The durability lane** is the `durability` profile: a host-scaled weighted pool
with the same 240-second slow timeout. Its members are crash matrices and the sqlite
SIGKILL commit campaign. They are slow because they relaunch a process and
fsync through a crash cut, and they depend on OS signal and fsync behavior, so
the core rows run in Linux batch and the platform-sensitive selection runs
in macOS batch. Mac installation checks (`just install-checks`) and
`just full` on both platforms run the complete lane.
The gate derives `N = max(1, min(16, available_parallelism))` with the same Rust
API the campaigns use, including affinity and CPU quota constraints. A tiny
native tooling probe runs before the lane; it opens no product runtime. The
reviewed source profile describes sixteen slots; `nextest_layout.py` renders
a gate-local copy with N slots and proportionally scaled integer reservations.
It verifies that every non-resource setting, including membership, watchdogs
and priorities, remains identical. The gate pins the rendered width on the
command line against environment overrides.
Independent cuts normally use one slot. The
sixteen non-switch restart cut groups have priority 90 and reserve
`ceil(N/4)` slots each, limiting simultaneous real-process restarts
without serializing them. The host start-scan campaign uses the same
reservation: it shares its immutable baseline commit list while each cut
creates its own fixture.
Weights are scheduling reservations, not fewer cuts, extra retries or relaxed
assertions. The existing extended watchdogs for host-restart matrices remain.

**The install lane** uses the same host-scaled width. The internally parallel
`durability_subset` and `durability_twin_pilots` campaigns reserve the whole pool
at priority -50. They fan out to N internal workers: the full reservation
prevents oversubscription and requires all earlier running work to finish;
the low priority starts the campaigns after shared work. The closed-cell ledger
twin-pilot campaign reserves the whole pool at priority 100 because competing
load changed its per-store signatures. Its isolation rule precedes the general
quarter-pool ledger weight, which starts the other ledger campaigns early
to avoid a long tail. Image residency and former serial families reserve
the whole pool at priority -50. Switch restart groups, the redial test and the
busy-root absence test reserve `ceil(N/4)` slots at priority 90.

| Install family | Evidence |
| --- | --- |
| Upgrade | `host_skeleton::upgrade::`: planned handoff and recovery, compatibility preflight, stopped residents and lifecycle notices |
| Ledger retirement | Four bounded retirement campaigns, including twin pilots and first-product-publication relaunches |
| Indexed durability | `crash_sweep::durability_subset` cuts every indexed commit in four shapes; `crash_sweep::durability_twin_pilots` checks equal per-store commit sequences for the other shapes without cutting |
| Image residency | Indexed image-residency recovery cuts |
| Runner startup | Redial bound, broken or obsolete daemon, slow user manager, surviving busy-root child and surviving child daemon job |
| Wire restart switch | All sixteen switch restart groups |
| Bridge | Auto-start idle exit, log-lock retry and typed readiness/stopping refusals |
| Operator pins and rate | Runner pin/control reconciliation and the real console-rate fixture |
| Lifecycle deletion | Lease keepalive and archive/delete admission |
| Placement hold | Reservation, instruction-drain and rebind cuts (`placement_hold_cut_`) |

`.config/nextest-install.toml` owns family membership. Every gate run of the
install profile selects all families; path changes do not select a subset.

Fixed absence/liveness observation windows retain their lengths under this
layout. `a_busy_roots_child_job_survives_attach_after_host_loss` observes a
live held child for five seconds in each of six cases: both hold and continue
for a crash and for a cut before root handoff, plus one clean handoff and one
cut after root handoff. `a_childs_daemon_job_survives_host_loss_and_handoff`
observes for five seconds in each of three cases: both policies for a crash,
and one clean handoff. `runner_wake_live_foreground` observes a
paused daemon's live job after 1.1 seconds.
`a_transient_daemon_failure_keeps_the_redial_bound` requires the host to stay
alive and report a blocked runner after a two-second redial window in each
daemon mode, shortened from the ten-second production window by a test hook.
The start-scan and busy-release `host_crash_cuts_` matrices
also check no new continuation/dispatch after 300 ms per indexed cut.
The Python and JavaScript `cell_worker_delivery_crash_cuts_preserve_only_proven_acceptance`
cases check no nested replay over 20 ms per delivery cut. Crash-sweep recovery
helpers check no second child continuation, grandchild dispatch or fenced-child
dispatch after 500 ms per cut (`shapes/child.rs`, `shapes/lineage.rs`,
`shapes/recovery.rs`); their internally parallel campaigns already reserve the
whole nextest pool, not a quiet internal worker.
The accepted trade-off is to keep these windows on their existing reservations,
not add a whole-pool phase exceeding a minute: descheduling can defer the
forbidden event beyond the observation window and reduce detection sensitivity.
This is not permission to shorten absence windows or retry failures. Lease-expiry
sleeps (three seconds with a two-second test lease in the actual-host and
account-continue SIGKILL matrices, and 600 ms in runner link faults),
delayed reconnection/readoption, lower-bound hold checks and polling sleeps
are not absence windows and keep their existing scheduling.

The tooling lane asserts that each restart family has all sixteen root/child
groups in its assigned profile; no per-run inventory pre-pass is needed.
Explicit steps may set
`PLAK_NEXTEST_PARTITION=count:K/N` (nextest's one-based partition syntax) for
`tests-parallel` and `tests-durability`. Each durability shard retains its
host-scaled reservations. Gate evidence requires all N shards on the same
tree, test target and selection; a single shard is not a passing
lane. Serial and install tests, named tiers and ignored retirement soaks are
not sharded.

Serial, durability and install remain sequential groups. Serial tests measure
real deadlines, cancellation and PTY responsiveness; a separate lane would
bypass its slot reservations and add internally parallel crash workers
to those measurements. The weighted durability evidence is not evidence for
overlapping the pools. The gated serial width is four on Linux and one on
macOS; ignored full-tier retirement soaks in both profiles still run alone.

| Kind | Predicates |
| --- | --- |
| Real SIGKILL relaunch | `actual_host_sigkill_relaunch_matrix_uses_real_handlers_and_reconcilers` |
| Continue on another account | `account_continue_sigkill_cuts_reopen_without_resubmitting` — a real SQLite home, the root's first turn failed on its usage limit, then `ChooseAccount` with `continue_failed`; SIGKILL after the rebind's `ProductPublished`, the root-start's `Prepared` and its `RuntimeReturned`, then two relaunches on the root line's head: one rebind, no physical dispatch and no submission by either reopen, a published-but-unqueued continue has no submission and one warning per open with the derived command id, a prepared root-start stays queued with its one written submission, and a returned one has exactly one accepted submission and no queued copy (chapter 02 §4.4.1). `account_continue_crash_cuts::` cuts the same boundaries in process |
| Spawn on a chosen account | `spawn_account_receipt_replays_after_relaunch` — cut the caller after the SQLite tree publishes the receipt and before child binding, reopen the persisted tree after a choice change, then replay the exact receipt: no new node, and the bound runtime account equals the node's original frozen reference (chapter 03 §2.1) |
| Real JSONL face SIGKILL | `jsonl_face_cut_before_the_turn_reopens_exactly`, `jsonl_face_cut_inside_the_model_request_is_abandoned_with_the_billing_note`, `jsonl_face_cut_inside_a_running_tool_is_abandoned_with_the_workspace_note`, `jsonl_face_cut_after_the_answer_reopens_exactly`; `jsonl_face_cut_inside_an_approval_is_asked_again_on_reopen` is explicitly ignored under `JSONL-APPROVAL-RESUME` |
| GC after a compaction cut | `gc_after_a_compaction_cut_before_projection_prunes_at_the_compacted_head`, `sigkill_gc_cuts_preserve_queue_counters_and_oracle` |
| Runtime commit cuts | `every_runtime_commit_cut_reopens_without_redispatch` |
| In-place store revisions | `host_skeleton::revision_cuts::` cuts the real host immediately before and after each store's commit; an older host leaves every database untouched, and the destination completes a mixed-revision directory without rerunning committed steps. The same-key upgrade lane exercises scratch-only qualification and planned handoff over a real root. |
| Crash-matrix completion control | `complete_run_is_one_exact_new_state_and_probe_is_inert` — the same harness child and evidence probe, without SIGKILL; the bounded child lifetime is the load sensitivity |
| SQLite paid-compaction SIGKILL | `sqlite_paid_compaction_sigkill_reopens_exactly_once`, `sqlite_no_locator_paid_compaction_sigkill_is_never_redispatched` |
| SQLite SIGKILL commit campaign | `bounded_async_sigkill_campaign_accepts_only_exact_old_or_new_commit` |
| Runner link faults and line ownership | `runner_killed_mid_job_restarts_with_its_owner_floor`, `host_killed_mid_call_is_recovered_and_readopted_by_its_host_id`, `host_crash_with_a_sibling_connected_orphans_only_its_lines` (a sibling process of the home stays bound while a holder is killed: it cannot stop the line's job with the owner it can read, and the line is orphaned at the dead holder's lease), `runner_restart_sibling_races_floor_owner` (a sibling binding first after a runner restart cannot take the floor owner; the floor's holder can), `crash_after_takeover_before_transfer` (a successor's durable newer owner never reached the runner; the next successor's newer owner takes the line and nothing wedges) — a runner daemon and a host in separate processes, each killed in turn (chapter 01 §8.4) |
| Cross-runner fork stages | `cross_runner_fork_stages` — over two loopback runners, the process ends once each mint and seal-back stage (chapter 03 §14) is published and again once each has run before its answer is recorded; the reopened tree settles without a fault and the rerun converges. It is in process rather than a crash-sweep shape because the sweep drives one product process, whose port reaches only its selected runner |

The paired terminal-history upgrade probes wait until the source checkpoint
records both the terminal job state and the completed tool result; disappearance
of the process alone does not imply durable publication.

In the runner link host-kill cases, the effect itself marks that it has
started on the runner and waits for release after the host is reaped. A
host-side journal entry alone witnesses intent, not dispatch.
The placement-hold host-loss case arms the successor's Ready barrier
before startup and resumes the daemon before the successor transfers
ownership. Only the source uses the deliberately short link windows that
induce the hold.

`offline_e2e::durability::` stays in the serial lane, as that lane's table says.
Every product and tree revision step ships with before- and after-commit crash
cuts, reopen and older-build unchanged-byte refusal tests, and a test that the
fresh and upgraded databases have the same pinned schema fingerprint.
`unsupported_runtime_schemas_refuse_without_changing_files` and
`real_binary_refuses_unsupported_runtime_without_moving_state` are not members
of either lane; they run in the parallel lane.

**One partition.** The `offline` profile excludes the union of serial,
durability and install. A run of all four lanes executes every non-ignored
test once. Production always requires the complete install profile on its
exact tree, alongside Linux core durability and serial from batch, the full
Linux parallel profile at install, and all four profiles at Mac install
checks. Change the owning filter or install family once, then run
`python3 tools/gate/nextest_layout.py --sync` to regenerate the offline and
install filter blocks in the checked-in nextest config. Tooling verifies the
blocks are current, proves their predicates resolve and do not overlap on
test definitions, and checks that reviewed timeouts and reservations remain
in force with no retries. The gate pins the reviewed widths against
`NEXTEST_TEST_THREADS` overrides.

The lane makes
`completed_terminal_outcome_remains_authoritative_when_exit_was_requested` quiet,
not correct: it is a genuine race in `crates/hosting/hosted/src/runtime/turn.rs`
("authoritative result was not ready") that has no readiness gate today. It is
listed
here rather than buried in a retry.

### 2.4 Goldens live in the crate

The durable tool-descriptor golden checks each real process registration against
both enrolled shell-profile bindings, then substitutes a fixed test-only process
binding for serialization pins. Shell device/inode authority varies by host;
it is not a portable format constant. Actual IDs, definitions, other bindings
and nested eligibility remain unchanged in the fixture. No production binding
or containment descriptor is normalized.

| Oracle | Where | Re-pin with |
| --- | --- | --- |
| Wire identity goldens (host-protocol intents, observations, views) | `crates/hosting/host-protocol/src/wire_identity_goldens.rs` | `PLAK_PRINT_WIRE_PINS=1 cargo test -p plakous-host-protocol wire_identity -- --nocapture` |
| Durable tool-descriptor golden | `crates/harness/compose/tests/integration/durable_descriptor_identity_golden.rs` | `PLAK_PRINT_DESCRIPTOR_PINS=1 cargo test -p plakous-compose --test integration durable_descriptor_identity_golden:: -- --nocapture` |
| Pack prompt pins (each pack's `pins()`, checked by `drifted_pins`) | `crates/core/tools/src/prompt_digest.rs`, each pack's `crates/harness/tool-{files,search,process,plan}/src/text.rs`, `crates/harness/compose/src/text.rs`, and `crates/orchestration/recursive-subagents/src/tree/text/mod.rs` | the failing assertion prints each drifted id with its new digest |
| Rendered route, lifecycle and wire pins | `crates/hosting/hosted/tests/common/{rendered_prompts,rendered_lifecycle,rendered_wire}.rs` | `PLAK_PRINT_PROMPT_PINS=1 cargo test -p plakous-hosted --lib rendered_ -- --nocapture` (§2.5) |
| Observation parity goldens (the host loop and the JSONL projection emit the same sequence) | `bin/plakd/tests/parity/*.jsonl` | `PLAK_UPDATE_GOLDENS=1 cargo test -p plakd --lib observation_parity` |

### 2.5 Prompt renders are pinned in code, not in files

What a model sees on each product route — harness costume, identity, enrolled
`AGENTS.md`, the skills listing (one fixture project skill and one user skill,
with synthetic roots), every tool name and description, the first-turn HTTP request
body, and each wire's compaction summary request body — is pinned by sha256 tables in `crates/hosting/hosted/tests/common/rendered_prompts.rs`
and `rendered_lifecycle.rs`, with the wire lowering in `rendered_wire.rs`.
Review headers assert the plakd User-Agent shape `plak/<semver>` separately and
normalize its release version to `plak/<version>` before rendering and hashing;
a workspace version bump alone does not change these pins. The suites
are private fixtures compiled into the `plakous-hosted` unit-test target,
the crate that builds a session
from route, composition, packs, topology and prompts together. The
tests render in memory and compare digests. They read no file, so a fresh clone
passes with no artifacts present.

The human-readable render is a local review artifact:

```text
just prompts render  # PLAK_UPDATE_GOLDENS=1 cargo test -p plakous-hosted --lib rendered_
just prompts diff    # copy the previous render aside, re-render, diff -ru
just prompts pins    # PLAK_PRINT_PROMPT_PINS=1: print the new pin tables
```

Output goes to the git-ignored `target/prompt-renders/`, overridable with
`PLAK_PROMPT_ARTIFACT_DIR`. Without `PLAK_UPDATE_GOLDENS` the same test still
performs the full render and checks the pins; it just writes nothing.

Beyond the digests, the tests assert file-free structural facts: the route
matrix is exactly 95 included routes plus two documented exclusions
(`native-ptc` is enrolled only as Codex Code Mode, and direct API routes render
only the single-agent root); the rendered wire JSON equals the
kernel-lowered request and the Markdown `## Request` fence equals the jsonc
formatter's output; the harness footprint excludes the enrolled `AGENTS.md`, the
skills listing and the scenario assignment, subtracts enrolled source bytes, and keeps the Claude
costume identity on Claude routes; and every product tool extension's name and
description reaches the model verbatim, either as a lowered `tools[]` entry or
in the harness instruction text.

The same suite holds the two normative harness-footprint budgets (chapter 02
§6.7): a basic single node under 4,000 approximate tokens, the fullest surface
at or under 8,000. GPT-6 Codex Direct/native Code Mode instead has an explicit
family budget under 5,500 for a single node and at or under 8,000 for the
fullest surface/children. Codex native-PTC single-agent leaves at least 150
tokens of headroom under the applicable family budget (5,350 for GPT-6,
3,850 for the GPT-5 base). `view_image` is not on the native Code Mode
outer surface. Other routes' budgets do not change. The suite also checks
GPT-6 versus GPT-5.6 base selection and pins the source and approved derived
bases; unaffected routes retain their rendered digests.
`rendered_prompt_footprints_stay_within_budget` fails the
build for any route over its budget, naming the route and its instruction and
tool split, and prints a table of every route's footprint whether it passes or
not, so the cost of a wording change is read rather than re-derived.

A prompt change is meant to fail the test with the list of routes whose digest
moved. The workflow is: `just prompts diff` to read the model-visible sentence
changes, then `just prompts pins` and paste the new rows over the tables. A
rendered artifact is never edited — `just prompts render` overwrites it from Rust.

Vendor CLI captures under a gitignored local capture directory come from
`just prompts native-compare`. Installed binaries run with isolated configuration
and synthetic credentials against local mock responders. Claude is driven
through its interactive CLI, not the print/SDK entrypoint. Its synthetic OAuth
request retains the original API origin through a local CONNECT recorder.
Codex control-plane calls use `chatgpt_base_url` on the local mock. The
responses turn uses `openai_base_url` ending in `/backend-api/codex`: an unset
provider base stays on `https://chatgpt.com/backend-api/codex` and never
reaches the mock.
The recorder rejects unknown CONNECT destinations rather than forwarding them.
Captures redact authorization, cookies, API keys, OAuth secrets and e-mail
addresses. They record the installed version and capture conditions; synthetic
identity and mock responses do not qualify live authentication, provider
acceptance or refusal recovery. These are comparison witnesses, never runtime
inputs or a gate. An existing CLI request log may also be ingested explicitly;
ordinary native debug logs are not guaranteed to contain request bodies.
Explicit qualification/reference OAuth/API wire captures belong in gitignored
`tests/provider-authority/`; ordinary maintainer capture belongs in
`PLAK_HOME/wire-captures/` (prefer a private home outside the checkout);
§3.5 is the safe collection and sanitization procedure. Do not commit raw
traffic or treat a local structural capture as live provider qualification.

plakd's private exchange observer is available in `maintainer` or `test-support`
builds. Maintainer builds select fresh private capture automatically in the
product home, including private scratch homes inside the checkout. Selection
is lazy: the first inference or read-only model lookup exchange creates and logs
the run, shared by later roots and reattachments in that process. Commands that
send neither create no capture directories and log no capture-selection message.
Catalog observers never attach to OAuth, token, profile or usage clients. Explicit
capture validation likewise happens before that first exchange can dispatch.
Automatic capture is passive and never blocks inference or lookup on recording failures.
Maintainer startup enables safe `plak.capture=warn` file diagnostics
in `PLAK_HOME/logs/plak.log`, including alongside `PLAK_LOG` target selection,
not application-client stderr. The live smoke and soak drivers also require capture for every process and default to
release executables. Explicit `PLAK_TEST_EXCHANGE_CAPTURE_DIR` selection in either
feature build is instead required, fail-closed qualification capture.
Capture persists mode-0600 `.request` bodies and upfront
`.info` crash markers before dispatch, then `.body` consumer response bytes and
terminal metadata, syncing request and terminal evidence and the directory
links for newly created capture runs. Pending captures
remain incomplete. The required observer refuses dispatch on start failure
and reports response/terminal IO or capacity failure to the transport.
Request and response limits are 8 MiB each; neither mode has a 512-start lifetime
limit. Automatic capture rotates segments at 128 starts or earlier under byte
pressure, sealing a partially filled current segment, and budgets 1 GiB per
process run (request bytes, actual response bytes, 4 KiB metadata allowance per
exchange, maximum response reservation while in flight). Before declining a new
recording, it reclaims successful exchanges from the oldest closed segments in
that active run. Each selected observation must have been dropped after `Complete`
or validated typed-terminal `ProtocolComplete` with `incomplete=false`, and no
HTTP error status. Arbitrary `ClosedEarly` does not qualify. Unsealed current
segments and pending, abandoned, failed and active observations are protected;
a protected exchange does not pin its successful neighbors. A private, synced `process/retention.info` records
merged actual exchange ranges selected for removal and `full_run=false` before deletion;
an interrupted cleanup may retain some selected files. This summary prevents a
retained suffix from being mistaken for full-run evidence. If protected/current
evidence exhausts the unavoidable disk limit, it declines
recording, persists a sticky `process/incomplete.log` and diagnoses incomplete
evidence, without refusing inference. Failed starts conservatively retain their
reservation. Startup retention keeps the eight newest
successful unlocked automatic-v1 runs, ordered by marker modification time and
then by run name, whose UUIDv7 follows creation order (within one process, and
to the millisecond across processes), so runs sharing a coarse timestamp expire
oldest first. It also keeps all locked active runs and all runs
with pending, failed, non-success or incomplete `.info` metadata or `incomplete.log`.
Retention inspects only bounded private metadata, never raw bodies, and preserves
unsafe or unrecognized layouts. Locks remain held by
in-flight observations. Legacy/explicit/reference directories without the
automatic-v1 marker are untouched. Retention failures preserve remaining files
and diagnose failure; many active processes, preserved failure evidence, unmanaged evidence and failed
cleanup can exceed the target. Explicit qualification has no run budget or
automatic retention; its driver owns that lifecycle. Metadata correlates run
path, process ID and process-wide exchange number; a failed terminal carries the
transport's redacted failure facts (spec 01 §13.1: phase, transmission, kind, cause
category, HTTP/2 reason, OS error number, TLS cause and alert code, negotiated
ALPN, HTTP/2 I/O origin, connection use) and nothing else; every `.info` also
records `connection=` as `Some(Fresh)`, `Some(Reused)` or `Some(ReplacedStale)`,
or `None` when no response head arrived or a test fault injected the response,
and the capture warning names it; the body-only observer
boundary has no session/turn identity. Pending markers, failed exchanges and
run-level gap or retention markers prohibit a claim of complete wire evidence, regardless of
inference success. This is not a recording of every network byte or token
refresh; no HTTP/auth headers are captured. Bodies remain private task data.
Codex's typed terminal explicitly closes and joins its body without waiting
for network EOF; a pooled client keeps the connection only when that end was
already received (spec 01 §13.1); required terminal-recording failure prevents success. Required
capture failures on refusal bodies remain transport failures, not retryable
provider status responses.

### 2.6 Spawned-binary lifecycles

#### Process ownership and synchronization

Real-process fixtures own the child and its process group before waiting for
readiness. Drop, panic and timeout must join the group and reap home-local
daemons before deleting the home. Signal delivery alone is not a join: a
grandchild must no longer be able to run, even if its zombie PID is still
visible. Failure injection targets the recorded owner, not a transient probe
or redial successor. Teardown re-lists successors in its own home.
Signal tests wait for the fsynced provider-dispatch record, after installation
of the signal watcher, not for an arbitrary elapsed interval.

The guarded entry points are the Rust lanes in `tools/gate/dev-check.sh`,
`just nextest`, and `just dev crash-sweep`. `tools/dev/coverage.sh` and
`tools/perf/perf-smoke.sh` are not guarded.

On macOS the default per-checkout base is `/private/tmp/<8hex>`, keyed by a
checksum of the UID and physical checkout path, with the same ownership,
0700-mode and symlink checks as the shared scratch selector. Each guarded
invocation creates `TMPDIR/r<base36-pid><2hex-nonce>`, holds its run lock and
passes that descriptor to the child, and gives
the child that directory as its test scratch and isolated home. Concurrent
invocations never reap locked runs, including when a child retains the lock
after its guard dies. At every startup it reaps only unlocked earlier runs,
reports those leftovers without failing the new command, and removes a run
directory only when no same-user process has its cwd beneath it. A successful,
leak-free command removes its own unoccupied run before releasing its lock;
the guard never resets a live run's files. A leak from the current run
fails that command. Only same-user hosts and runners inside its run directory
(including deleted working directories), and their owned descendants, are
signalled. Hosts stop before daemons and cleanup re-lists successors launched
while a host dies. TERM, INT, HUP and QUIT are forwarded unless inherited as
ignored (including HUP under `nohup`); ignored dispositions stay inherited by
the child. Forwarding does not preempt the child's cancellation grace; only a
second signal escalates to KILL. Interrupted
cleanup skips the normal settle wait. The guard cannot clean up if it is itself
SIGKILLed; an unlocked surviving run can be reaped by a later invocation.
The tooling socket-budget check reserves at least 12 bytes below macOS's
103-byte path limit for the current runner-build, home-local, host-control,
web and remote fixture suffixes, including maximum PID and fixture-ordinal widths.

#### Offline faces and fixture controls

The product integration suites spawn real `run`, `agent` and `host` processes
against an offline scripted model. They prove exit status, cancellation,
non-reading peers, ordered events and exact reopen across processes; a unit
test cannot prove these. There is no provider network or credential authority.

- A `test-support` binary accepts `PLAK_TEST_MODEL=offline-loopback-v1` and an
  absolute `PLAK_TEST_SCRIPT` pointing to a `ScriptPlan`. A minimal script is
  `{"version":1,"repeat_last_step":true,"steps":[{"output":{"text":"offline answer"}}]}`.
  Optional `PLAK_TEST_EVIDENCE` is an absolute JSONL output path.
  Use an isolated home and `PLAK_TEST_RUNNER_DETACHED=1` for a home-local
  runner without a user service manager. An unusable requested hook refuses;
  never fall through to real inference.
- The exact selector makes capacity Available. The independent
  `PLAK_TEST_STORAGE_CAPACITY=available|warning|refused` override refuses
  invalid values and lets doctor fixtures avoid actual free-space dependence.
  In-process capacity fixtures use the store injection seam; ordinary builds
  use the real probe.
- Only the scripted Codex account/model row becomes Ready, with low and medium
  effort and no priority tier. Scripted profile switch, fork and reopen use the
  normal binder over that exact route. Effort remains binding, unchanged exact
  runtime still refuses, and no unrelated account/model becomes Ready.
- Scripts vary model rounds, faults, usage and folds through the same offline
  model and boundary controller as the in-process fixture. Frozen/selected
  optional tools go through the product binder; Direct and nested Code Mode /
  JS catalogs preserve MCP ownership, including after compaction or rebind.
  Fake MCP call/list markers distinguish once-only dispatch and frozen listing
  from recovery replay. A child fault does not imply refused root admission.
- `tree_process_tools` requires `tree` and adds the standard process tools to
  that scripted registry. Child controls require `tree`;
  `disk_full_at_child_locator` and `product_child_script` require
  `durable_tree`. Real product children (`product_child_script`) exclude
  scripted-child fault/hold dials, use the actual child path for bind/reopen and
  append requests to `child-evidence.jsonl` alongside root evidence.
- `PLAK_TEST_PROVIDER_RELEASE` gates first physical dispatch and respects
  cancellation. The script's `identity_hash_delay_ms` delays hashing, not
  identity or bounds.
  With `--timeout`, an existing `PLAK_TEST_RUN_DEADLINE` file reaches the
  ordinary interrupt/deadline path; without `--timeout` it does nothing.
  These are scheduling controls, not product policy.

| Fixture control | Effect or witness |
| --- | --- |
| ScriptPlan `fold` | Answers or refuses a semantic-summary request. A refusal takes the offline placeholder rung. |
| ScriptPlan `effect_decision_timeout_ms` | Overrides the scripted runtime's effect-decision deadline for expiry/refusal cases, never the production default. |
| ScriptPlan `child_hold_ms` / `child_hold_after` | Hold a scripted child for the given milliseconds after publishing its locator, once that many earlier children have bound. A count of zero holds the first child; a zero duration disables the hold. |
| ScriptPlan `fail_child_after_successes` | Children whose zero-based bind ordinal is at least the configured threshold N return recovery-required; earlier children complete. |
| ScriptPlan `stall_ignores_cancellation` | Opts a scripted stall out of its normal cancellation observation. |
| `plakous-mcp-fake-server --call-log PATH` / `--list-log PATH` | Append the called tool's name before it runs, or one `list` line per `tools/list` request, respectively. These cross-process witnesses distinguish dispatch from listing and recovery from replay. |
| `PLAK_TEST_ASSISTANT_DONE_RELEASE` | Only applies with the offline model. Gates the face after writing `assistant_done`, before checking stdin. Atomically release with `quit` to wait for the already-sent input, `release` to continue without input, or `idle` to drain through the real `busy:false` observation first. |

Two explicit client-test producers require `test-support`. The setup-only
`plakd --test-seed-abandoned-open without-submission|with-submission` command
requires the offline selector/script, an explicit absolute `PLAK_HOME`, an
unused runtime store and an isolated workspace. It settles `seed terminal`
through the ordinary offline worker, quits and joins that worker, then calls
the public fenced-code-cell harness seed with command `face-fenced-root`, input
`fenced face prompt` and `finalized_root=false`. The with-submission variant
alone saves the matching RootStart submission and pending queue item. Its one
JSON setup receipt names only fixture/session identity, variant and sanitized
provenance, never a recovery observation. It exits without reopening. The
subsequent ordinary host/bridge resume owns the actual typed `Recovered`,
projected recovery row, binding, baseline, queue and history observations.
For the real-host journey, `PLAK_TEST_RECOVERY_FIRST_ATTACH=<receipt session>`
holds only that session's initial wire observation pump until an actual node
attachment exists (120-second safety bound), rather than losing opening events
before the first client can attach. The worker still opens normally. This
test-only scheduling seam neither synthesizes nor rewrites any observation;
subsequent resync and reattachment use the unmodified host paths.
Public oracles retain the JSONL once-only and derived-not-durable row checks
for both variants and compare physical dispatch counts across open/resync.

With the offline selector, all three of `PLAK_TEST_DRAFT_PENDING_ID` (one
client draft UUID or the exact selector `first`), `PLAK_TEST_DRAFT_PENDING_READY` and
`PLAK_TEST_DRAFT_PENDING_RELEASE` (distinct new absolute file paths) install
a one-use reservation hold. A UUID selects only its first actual reservation;
`first` selects the first actual eligible root draft reservation, preserving
the native sender's original UUID even when minted after host startup. Reads,
session creation and attachment do not consume the selector. Fixed-ID mode's
ready file remains exactly `reserved` plus newline. First-reservation mode
writes `reserved <original-uuid>` plus newline, providing a content-free reached
barrier and identity witness, not a replacement ID or an admission verdict.
Neither mode can hold a second reservation. No draft content is logged.
The test-support `test_draft_gate::install` accepts an optional selected ID
(`None` selects first) and passes the original reserved ID to its wait callback.
The already-authorized original sender and unchanged intent
wait outside the host loop and service locks. Directory reads, attachment,
reconnect, conflicting-content refusals and real same-ID `draft_pending`
checks remain live even after the original connection disappears. A release
file published by atomic rename and containing exactly `release` with optional newline sends the original
intent once; any other content, a terminating signal or the 120-second safety
deadline drops the reservation without admission. Failed mailbox delivery uses
the same reservation-release path. Durable admission alone enables a later
real `accepted { duplicate:true }`; this seam creates no synthetic verdict.
Native-first-write evidence mints the UUID after host startup, observes that
same UUID at the barrier, drops the original connection, obtains repeated real
pending and conflicting-content refusals after reconnect, then releases the
original for one durable admission and provider dispatch. A later independently
minted draft bypasses the spent gate. Cancellation laws also run in both modes
and require no original history/dispatch before an explicit replacement.
The product reads none of these controls in default or maintainer builds;
the hosted hold port is compiled only with `test-support`, already excluded
from both production resolutions. Binary-marker/CLI contracts prove exclusion.

`plakd/test-support` is absent from default, maintainer and release builds.
Workspace and feature checks pin its edges to the harness, hosted scripted
factory and tree-store trace forwarding (chapter 06 §4). The normal construction
port remains available without them. The harness supplies the product's only
edge to transport fault injection (§2.8). No test selector adds persisted or
wire vocabulary.

**Code Mode enrolment is harness-only and says so in its own identity.** Local
Code Mode compiles only for a model whose projection target the composition
enrolment recognises, and the scripted model is not any real provider's model.
`plakous-compose` therefore carries one extra enrolled target behind the
`scripted-code-mode-enrolment` feature, which is off by default and reached only
through the harness. The target's every field is a `scripted` or `offline`
identifier — `offline.scripted.code-mode.responses`,
`offline.scripted.code-mode`, `offline-scripted-code-mode-v1`,
`offline-scripted-code-mode-adapter-v1` — so a scripted session can never be
mistaken for an enrolled Codex one, and the feature adds exactly one branch to
the gate while leaving the gate's own conditions byte-identical. No Codex model
name, adapter revision or route appears anywhere in it.

The harness does not mint a `ResolvedProvider`. It builds the composed
configuration directly: the provider configuration owns exactly the two markers
in the `LocalCompositionOnly` presentation, the nested catalog stays in the
registry, and the harness plan comes from `compile_local_composition_plan`, so
the provider-visible exposure is empty. A composition deployment exposes its
catalog through the evaluator, never to the provider, which is why those two
facts are consistent rather than contradictory.

#### Authority and convergence families

| Family | Required observations |
| --- | --- |
| Remote runner and skills | Workspace and user-root discovery is through the runner; exact reopen keeps frozen skills. Bounded slices must preserve file identity across reads and refuse escaping links, torn pages and unavailable runners, not read host fallbacks. Stop may return without a held link's answer. |
| Host lifecycle | Real signal, blocked reader and failed-shutdown evidence checks bounded joins, published locators and terminal stream behavior. UI asset reload keeps the same session WebSocket on `SIGUSR1` and retains the previous assets on refusal; `SIGHUP` is planned handoff. |
| Runner reconnect, startup and stop | Check exact physical owner and command receipts through acquisition, foreground/job transfer, terminal recording, job adoption and keyed stop. An answered start is not a committed terminal. Preserve the matching process and start environment on live reconnect; do not recreate it after proof is lost. Sweep all lines and collect failures; held work cannot block unrelated cleanup. Embedded open must not adopt running runner work implicitly. |
| Host loss, release and handoff | Busy handoff continues at most once from its own durable decision; an unrecorded turn remains held and an idle handoff cannot borrow an old decision. Release does not turn an answered turn into loss. Real holders fence takeover until proved gone, with exact tree custody separate from runtime lease. A replacement cannot treat an absent daemon as proof that an orphan was stopped. |
| Product bind and residency | Rebind/retry-attach restart from the actual custody fence, release, carried history or opened target; keep one canonical terminal and one actor, without replay. Explicit retry after pressure keeps exact input/mail and submission identity. Full capacity refusal and image-residency fallback preserve the paid guard and cannot publish unstaged authority. |
| Durable tree and process work | Spawn, refreshed workspace, adoption and conflict resolution use real intent/feed boundaries. Retained copies keep pinned provenance, sealed artifacts and exact result mail once; live remote jobs retain their owner. The single-root sweep cannot enroll real child-turn recovery without multi-branch summaries and child-specific relaunch binding; the durability child matrix is separate. |
| Recorded decisions | A checkpoint atomically records each decision and its history, control, usage and budget facts. Completion races may leave a durable decision without its consequence, never half a consequence. Open publishes it once; `recovery_verdict` is the sole predicate for live open, inspection and rehearsal. Every closure-table row has a synthetic witness for claim, note, closed operations and exact usage. |
| Feed and projection | A commit appends its facts atomically; event and fact witnesses agree. Each row has an owning source; a stale cursor cannot overwrite newer derived rows or admit foreign work. A lingering owner's intents are revision-fenced, and its submissions cannot reach the provider. Incremental, clear-and-replay, and plain reopen produce exactly the same views. Export, GC, open and rehearsal share the same reconciliation path. |
| Projector idempotence | Apply every pilot feed and a generated corpus of well-formed feeds twice in both projectors, exercising every page boundary and a replayed page. The second application leaves byte-identical views and plans no further writes or successors; on converged real-host memory and SQLite views the cursor advances zero. Compare all SQLite view cells, including head and keyed bookkeeping, and refuse unknown tables in the snapshot inventory. |
| Capture versus inference | Read-only inspection must preserve main/WAL bytes. Runtime/test hooks and append-only dispatch evidence distinguish root, child, tool and paid summary operations. Provider-hosted tool results never cause local dispatch. A new explicit work request is distinct from replaying a recorded dispatch. |

The named hosted cuts are the variants of
`plakous_hosted::test_support_runner::HostCut`, public only with
`test-support`. `PLAK_TEST_HOST_READOPT_CUT` must name a variant; invalid
values abort rather than silently skipping a cut. The analogous SQLite test
enum requires exactly one SIGKILL matrix membership per cut. Exact-name lane
checks use full binary-relative paths, not bare module names or suffixes.

#### Indexed crash-point sweep

**Selection.** `just dev crash-sweep [--shapes SHAPES] [--jobs N] [--repeat R]
[--strict]` builds the shared workspace fixtures and runs the ignored full
sweep. The install subset cuts `final_only`, `direct_tool_then_final`,
`promoted_background_job` and `sealed_family_artifacts` with up to sixteen
workers. Other registered shapes run twin pilots in install. Separate bounded
retirement campaigns cover closed cells, interrupted batches, a live child
and process, and a yielded cell with delivered children below the retirement
boundary. Representative cuts include the first commit of each store; the
live-child/process and yielded-cell campaigns require a first product cut
and reserve the entire pool as specified in §2.3. Their ignored exhaustive
counterparts run in full/soak.

Test-support `PLAK_TEST_LEDGER_RETIREMENT_HIGH_WATER` (8–2048, default 2048)
and `PLAK_TEST_LEDGER_HEADROOM_LIMIT` (default 3584) reach retirement/refusal
without changing persisted bounds. Retirement pilots compare final ids/states
and tagged counters, alpha-renaming only independently minted root UUIDs
after checking their bindings. Pilot assertion failures preserve
`pilot.jsonl` and `pilot.stderr` with the fixture and name observed
record kinds and credential-wait rows.

Without `--shapes`, `--strict` selects every registered shape; a non-strict run
uses the quick `final_only,direct_tool_then_final` selection. An explicit
`--shapes` selection applies in either mode. The wrapper overrides any inherited
`PLAK_CRASH_SWEEP_SHAPES`, leaving it unset for the strict all-shapes default so
the Rust registry supplies the complete inventory.
`PLAK_TEST_WORKING_PRUNE_MINIMUM=1 PLAK_TEST_WORKING_PRUNE_MARGIN=1 just dev
crash-sweep --strict` runs every shape with the owner pruning between every
pair of rounds (spec 01 §4 history retention). The minimum knob lowers the
64-checkpoint batch; the margin knob lowers the 64-checkpoint recency floor,
and 0 or 1 both leave the horizon at the head's parent so a short turn still
prunes. The fixture forwards both variables to each spawned `plakd`. Both knobs
are for the crash sweep only, not the durability crash matrix or the offline
lane, and the parity tests run at the default margin of 64. A traced
build's SQLite actor drains deferred reclamation right after the job that
queued it rather than when it next idles, so mid-turn reclamation cannot race
the next round's commits and every twin pilot keeps one commit order; untraced
builds are unchanged.
One pool of `--jobs` workers runs every shape's setup, twin pilot and cuts:
shapes run concurrently, each fixture in its own home, and a shape's cuts are
queued ahead of the setups still waiting, so its seed is cut and removed before
more shapes are prepared. Rows are written sorted by shape, repeat, store and
ordinal, so `sweep.jsonl` and `table.md` do not depend on which work finished
first; while the sweep runs they are refreshed at most every 30 seconds, and the
final pair is written once every task is done. `timings.jsonl` beside them
records each setup-and-pilot and each cut with its start and duration.

To distribute a strict sweep, run `just dev crash-sweep --strict
--shard I/N --output RECEIPT` on the same clean revision and target, once for
each index from 1 through N. A shard runs the complete indexed cuts of its
assigned shapes, assigned runner authority cases, and all sixteen groups of
each assigned wire restart family. Both pilots and cuts of a shape stay
together. The deterministic assignment uses largest estimated shape cost
first and places each shape on the lightest shard; estimates affect placement,
not coverage. Shards use the all-shapes selection with one repeat and default
pruning. The driver and inventory must belong to the tracked revision; the
shard checks that the checkout is clean and unchanged, including untracked
files, before and after running the tests. Receipts record the Rust build
target, defaulting to the compiler's host triple when none was selected.

Run `just dev crash-sweep --merge RECEIPT... --output DIRECTORY
--expect-commit REV --expect-target TRIPLE` to verify the union against the
revision, its tree and the required target. The merge rejects mismatched
inventories; missing or duplicate shard indices, cuts, runner cases or
restart groups; and missing, failed or refused invariant rows. The checked
inventory identifies every expected per-store ordinal; default strict
pilots also verify the counts before cutting. Changes to the registry or
cut inventory update that inventory against an unsharded run of the affected
shapes, inspecting the pilot counts when they differ. A successful merge writes
`sweep.jsonl`, `wire.jsonl` and `coverage.json` in the output directory.

**Pilot and cut laws.** A clean post-setup seed supplies each fresh home.
Twins must agree on each store's indexed `(store, label, tables)` sequence
and on live effect/dispatch evidence. The terminal-input barrier follows
actual `busy:false` after the answer (and after any compact tool terminal),
not a stale idle record. The 45-second stalled-step deadline advances only
with face records or indexed commits, never lease renewals; the lifecycle
also has an absolute 450-second cap. Fail a success pilot on its first
unexpected failed terminal. Killing at `<store>:<k>` must exit by SIGKILL
with exactly the prefix through k on that store and a prefix of each other
store; inter-store ordinal and non-target progress may differ. Two
divergence retries precede a reported failure with the trace diff. Setup
authority is excluded from classification of the cut original.

| Sweep shape family | Additional obligation |
| --- | --- |
| `image_residency_admission`, `image_residency_tool_batch`, `image_residency_continue` | Seed protected image history and cross the mandatory aggregate-image fence with optional scheduled compaction disabled. Require `ResidencyPlaceholdersV1`, exactly one Finalized compaction and no paid compact dispatch. Cut every indexed runtime/product commit around the offline one-CAS publication; the install lane repeats these cuts and direct receipt tests independently check v3 source/selection/successor evidence. |
| `residency_dormant_materialize` | Run `plakd host --web unix:<path> --idle-release 1`. First attach answers once; after detach and loss of the live owner, a second attach rematerializes the session. Require one provider dispatch and two open/opened pairs. Cut every indexed endpoint commit, including idle release and wake, then recover through an ordinary `plakd agent` relaunch. |

**Reopen oracle.** Read-only rehearsal of the exact runtime head, recovery
obligations and ordered ledger, feed/cursors, product/tree views, sealed
artifacts and physical dispatches predicts closure and projection lag and
must preserve each SQLite main and WAL byte (not transient SHM). The first
relaunch closes/reconciles and runs an explicit follow-up. Assert that open
alone brought the cursor to the feed tail before any harness projector pass;
that pass must be inert. The second relaunch adds no dispatch or publication;
CAS revision changes from idle attach/quit are recorded, not new authority.
A refused relaunch must leave the entire decoded summary unchanged.

| Result | Required witness |
| --- | --- |
| `COMPLETED_ONCE` | The original command's one recorded terminal decision and matching answer row, or the recorded capacity denial; fixture text is not the decision witness. |
| `ABANDONED{claim}` | One honest typed interruption derived from the obligations, never overriding a durable answer. |
| `REQUEUED` | The original prompt remains in a paused queue, with a pending submission and no admission fact. |
| `NOT_ADMITTED` | No submission, command or answer for the original beyond the seed; clean reopen and one explicit follow-up dispatch. |
| `FENCED` | One `RecoveryRequired` terminal/claim for the original, from live closure or from its recorded outcome-unknown decision. |
| `REFUSED{name}` | Typed exit 5 and unchanged durable authority. No signature is allowlisted; any refusal is a finding. |

`UNCLASSIFIED`, divergences and invariant violations fail all runs. A refusal
can be accepted only by adding an allowlist row to this chapter naming its
exact signature and the genuinely uncertain external effect that no closure
can honestly settle. The allowlist is empty; product and tree signatures are
never eligible.

**All product and tree cuts MUST converge.** After relaunch one, cursors
equal their feed tails and the head view equals the runtime head; submissions and
workflow requests have a fact or remain pending behind a paused queue.
Relaunch two changes no views. A durable provider terminal/failure decision
finishes once; partial local evaluation/tool evidence is retained honestly
as abandonment and cannot authorize a replay. Do not duplicate dispatch,
derived terminal rows, usage or canonical notice consumption; observational
job snapshots may rehydrate once per attachment. A compaction without an
admitted summary withdraws with one not-started notice and explicit remedy;
admitted compaction recovers the recorded decision or its honest uncertainty,
with zero summary redispatch. Summary-specific fsynced evidence counts physical
dispatch, not ordinary provider turns.

The install subset requires root delivery coverage spanning prepared,
runtime-returned, projected and terminal-ready states. It does not replace
physical-process liveness at completed-tool-round, exact sealed-candidate and
mismatched-command finalization, reclaimed delivery, send-now, Compact, Fork
or Rebind cuts; component, GC-before-projection, facade and store-atomicity
matrices still stand. Retained real-host/GC lease waits exercise the actual
ownership fence, not a shortened timer.

Artifacts are `target/crash-sweep/<run>/sweep.jsonl` and `table.md` with
per-shape/repeat outcome counts and refusal signatures/cuts.

Compare two artifacts with `python3 tools/dev/compare_crash_sweeps.py
BASE.jsonl TIP.jsonl [--json]`. The cut key is `(shape, store, store_ordinal)`;
repeat numbers are samples, not identities. Within a key, comparison also
requires equal recorded pre-cut inputs: the exact indexed per-store prefixes
from `global_order` (labels and changed tables, ignoring cross-store order),
and the `before` ledger states and available phase, verdict, revision,
obligations and turn/tool counts. Random checkpoint identities and post-recovery
output are not inputs. An artifact with only ledger evidence can be compared
to another ledger-only artifact; absent evidence is never an equal input.
Matching outcomes and invariants are reported separately from genuine changes
under equal inputs. Samples at the same ordinal with different inputs are
floating cuts, not by themselves outcome regressions. Unequal sampling of an equal input,
missing cut keys and rows without input evidence are reported separately. Equal outcomes
are paired before differing outcomes so repeat reordering is inert. JSON output
includes the input fingerprints for floating cuts and genuine changes. Exit 1
means genuine changes or worsening tip failures, exit 2 means invalid input,
and exit 0 does not certify floating or missing cuts. Every tip sample whose
invariants are not exactly true, or whose outcome is `DIVERGENT`, `UNCLASSIFIED`
or starts with `REFUSED`, is also reported as `regressed`, irrespective of
its input evidence or pairing. For each cut key, failing samples are counted
by their exact outcome string. Each baseline failure covers at most one tip
failure of the same kind, irrespective of the input fingerprint. An additional
failure or a new failure kind makes the exit status 1 even when the base
already failed that key. Covered pre-existing samples remain visible with
`baseline_failed: true`; unmatched samples have it false. Fewer failures of
the same kinds do not themselves make the exit status 1.

**Commit trace contract.** The repository-only `plakous-store/test-support`
feature explicitly forwards `plakous-agent/test-hooks` for its read-only,
content-free checkpoint classification bridge. The classifier is absent from
default production builds. The
feature is enabled by the offline-crash harness; `plakd/test-support` directly
forwards the tree store's own `test-support` feature and carries four pinned
edges; default and maintainer/release product builds contain no commit tracer.
plakd's harness dev-dependency also enables runtime-only tracing in test builds
without `test-support`; a sweep must require `cfg(feature="test-support")` to trace
the product and tree as well. The
runtime, product and tree install it on their writable WAL connections, not the
telemetry database or read-only inspection connections. Runtime installation is
beside the existing test commit hook, after schema/domain admission; the two
hooks are mutually exclusive when tracing is active. Product/tree installation
is after WAL configuration and before schema admission.
All commits before installation are untraced. Traced connections must step
writes to completion before calling thread-scoped `commit_trace::settle()`.
A write whose commit completes during reset or finalize (`RETURNING` through
`query` or `query_row`) loses its WAL callback and is untraceable. SQL `RETURNING`
is therefore forbidden in the sources of all three traced stores.
`check_workspace.py` pins this limit by matching the keyword, case-insensitively,
inside Rust string literals, not comments. The conservative scan includes inline
`#[cfg(test)]` modules in those source files.
`VACUUM` is unsupported under trace. Settle runs only outside SQLite, after
transactions have committed or rolled back; it is not a mid-transaction API.

`PLAK_TEST_COMMIT_TRACE` must name a **new** writable JSONL file, one per traced
process (including each relaunch). Without it, installation is a no-op, retains
existing hooks/autocheckpoint, and ignores `PLAK_TEST_COMMIT_KILL`. With it,
`PLAK_TEST_COMMIT_KILL`, if present, must be `<store>:<k>`, where `store` is exactly
`runtime`, `product` or `tree` and `k` is a positive decimal integer. The global-k
form is not accepted. Each durable
WAL commit that writes frames appends and fsyncs exactly one JSON object followed
by a newline. Frameless commits emit no line and advance no counter:

```json
{"ordinal":1,"store":"runtime","store_index":1,"store_ordinal":1,"label":"commit","tables":[],"indexed":true,"classes":[],"retired":0}
```

The exact fields are `ordinal` (nonnegative integer), `store` (`runtime`,
`product`, `tree`), `store_index` (positive integer), `store_ordinal` (nonnegative
integer), `label` (string), `tables`
(sorted unique array of table-name strings), `indexed` (boolean), `classes`
(sorted unique content-free publication/anchor class strings), and `retired`
(the authenticated retired-local-attempt count, zero for non-runtime-commit
rows). Class metadata names kind/state transitions committed at that exact
checkpoint and retained anchor classes on either side of P; live jobs are
named without identities. SQL signatures retain their `(store, label, tables)`
contract. Retirement twins also compare per-store class and counter-value
coverage sets: concurrent child/parent connections need not interleave metadata
at identical merged ordinals. Their final ledger facts and encoded counter must
still agree. No row
values, paths, timestamps, connection IDs, prompts or provider data are emitted.
`store_index` counts all traced framed commits of that store kind across every
connection in the process, starting at 1. `ordinal` counts **indexed** commits
globally across all three stores, starting at 1; cross-store thread interleaving
can change its order between runs, so it is reporting evidence, not a kill
target. `store_ordinal` counts indexed framed commits of that store kind,
starting at 1. Runtime `renew` and `revive` commits have `indexed=false`, retain
the previous global and runtime indexed ordinals (0 before the first), and
are never kill targets; all other commits are indexed. Runtime dispatch closures
scope the label to the store method name; unlabeled commits use `unlabelled`.
SQLite's update hook supplies names only for rowid tables in `main`; WITHOUT
ROWID tables and schema changes can have an empty array. Tables are observational
touches, not an authoritative mutation inventory; rollback clears the pending
set, while savepoint rollback may leave touched names in it.

A process-global binary semaphore spans commit-hook admission through the WAL
callback, trace fsync and optional PASSIVE checkpoint. SQLite executes both
callbacks synchronously on the committing thread; thread-local pending context
bridges rusqlite's non-capturing WAL callback. At the target store's indexed
`store_ordinal=k`, the process
raises `SIGKILL` after the line is fsynced and before any later traced connection
can commit. At operation completion, `settle()` releases any remaining
thread-local claim as frameless, without emitting a line or advancing an ordinal.
Commit and WAL callbacks for a fully stepped framed commit run inside the same
SQLite call, so its claim cannot survive to that boundary. Runtime settles at
the outer actor operation completion, including errors and unwind, with a Drop
guard before replying. Tree store uses the same guard at both database-helper
completion boundaries. The guards are declared before work so transaction rollback
precedes settlement during unwind. Product transaction
operations and the two implicit acknowledgement mutations settle with a guard
declared before their transactions, so rollback/drop precedes settlement on
early returns too. A later commit hook on the same thread releases and reclaims
an outstanding claim; owning-connection acquisition is idempotent for BUSY retry.
Rollback releases only its own semaphore claim. A 5-second semaphore
timeout, invalid controls, trace I/O failure or inconsistent callback ownership
reports `DIVERGENT commit-trace` to stderr and exits 86, never an apparent clean
pilot. The WAL hook replaces autocheckpoint, so non-killing commits perform a
best-effort PASSIVE checkpoint at 1000 or more WAL pages, ignoring its result
just as SQLite's default autocheckpointer does.
The trace is evidence instrumentation, not a persisted product format.

### 2.7 The offline performance smoke

`just perf smoke` measures five short product paths over the scripted model,
four cell-worker timings and six retained-history metrics. It emits one JSON
line per metric:

| metric | what it times | compared |
| --- | --- | --- |
| `cold_start_to_ready_ms` | process launch to the `ready` record on the JSONL face | no |
| `jsonl_tool_round_trip_ms` | one tool row from `running` to `done`, median of five repeats | no |
| `cli_two_tool_turn_ms` | a whole `plakd run` process for a two-tool turn | yes |
| `jsonl_bulk_throughput_mb_per_s` | ten prompts, each answered with 100 kB, over the whole batch | yes |
| `growth_turn_peak_rss_mb` | maximum resident set over a growth turn through the placeholder rung | yes |
| `cell_quickjs_spawn_round_trip_ms`, `cell_monty_spawn_round_trip_ms` | worker spawn, checked hello/configure, one cell with a nested call and its acceptance, median of five | no |
| `cell_quickjs_per_cell_ms`, `cell_monty_per_cell_ms` | the same nested-call cell on a warm worker, median of 80 cells | no |
| `lineage_400_total_ms` | all 400 prompt turns plus the final actual-history verification turn | no |
| `lineage_400_first_20_median_ms` | median of the first twenty prompt turns | no |
| `lineage_400_last_20_median_ms` | median of the last twenty prompt turns, excluding verification | no |
| `lineage_1000_total_ms` | all 1000 prompt turns plus the final actual-history verification turn | no |
| `lineage_1000_first_20_median_ms` | median of the first twenty prompt turns | no |
| `lineage_1000_last_20_median_ms` | median of the last twenty prompt turns, excluding verification | no |

Volume comes from assistant text rather than from tool results, and that is a
product fact rather than a convenience. One tool outcome is bounded at a
mebibyte, the runtime refuses to group a pending exchange whose results run much
above two hundred kilobytes, and the face clips a tool observation before it
reaches a client, so a large tool body never becomes stream bytes. Assistant
text is published verbatim up to a few hundred kilobytes, so that is what the
throughput case uses.

Every line carries a build `profile` and a machine id derived from the platform,
architecture and CPU, because a number measured on one machine or profile says
nothing about another. Seeds live
in `tests/performance/baselines/offline-perf-smoke.json`, keyed by that id.
`--check` runs `tools/perf/check_perf_smoke.py` against this machine's row. A
machine with no seed row reports `no baseline` and its numbers, then passes.
Every seed metric also records its `profile`; a missing profile or a profile
mismatch is refused, including for informational metrics. The baseline document
may contain no machines: comparisons require an explicit idle-host reseed.
Profile mismatches use `perf smoke: incompatible baseline`, not the
`perf smoke regression:` prefix reserved for comparable timing regressions.

Five short product metrics are measured. `growth_turn_peak_rss_mb`,
`jsonl_bulk_throughput_mb_per_s` and `cli_two_tool_turn_ms` are compared with a
fifty percent tolerance by `--check`. `jsonl_tool_round_trip_ms` and
`cold_start_to_ready_ms` are informational: an isolated measurement cannot
distinguish a slowdown from host contention.

Seeding a new machine waits for one run in which all five metrics measure
successfully on an idle host, so a seed never records a contended number as the
thing to beat.

The recipe builds only `plakd`, `plakous-js-runtime` and `plakous-py-worker` in
the release profile, with the product's default features plus `plakd/test-support`.
It copies `plakd` and its matching JavaScript/Python workers aside before
measuring. Product and
worker timings measure optimized binaries; the row profile is `release`.
As in the product build helper, `CARGO_PROFILE_RELEASE_STRIP=debuginfo` retains
raw symbols for the artifact-isolation checker; generated code is release-optimized.

Additional informational cases retain 400 and 1000 sequential prompts in one
session. They report total turn time and first/last twenty-turn medians. The
final scripted request returns its actual input history and must contain every
earlier prompt, so dropping history cannot appear as a performance improvement.
These measurements have no threshold and are outside every gate.

A metric the machine cannot measure is reported as **no measurement**, never as
a regression. The resident-set figure is the case that forced the distinction:
BSD `time -l` reports bytes, GNU `time -f %M` reports kilobytes, and some images
carry neither, so the recipe probes for a usable one. Assuming BSD made a Linux
run abort and the gate call it a slowdown.

The smoke is in no gate tier; `just perf smoke` runs it on demand. A
measurement on a shared machine is evidence, not a gate. The restricted comparison refuses
to call a swing in either noisy metric a regression.

**Build-closure evidence.** The opt-in build producer emits
`plakous.perf-build.v12`, fixture revision 12, over 18 profiles. The
`plakd` and `plakd-lib` profiles select `web` explicitly and measure the
default product feature resolution. The unconditional product capabilities
make those measurements a new baseline, not comparable to v11's flagless
product profiles. Archived v11 measures `plakous-exchange` with the folded
attempt core.
Archived v10 retains its 18-profile contract, including `attempt` for
`plakous-attempt` and a single `responses` profile. Archived v9 retains
19 profiles with separate `responses-kernel` and `responses-host` profiles.
Campaign validation selects the exact profile set and fixture revision
belonging to its schema.

**Benchmark workloads.** Three deterministic offline `cargo bench` executables
are outside every gate (§1.2). `perf_core` (`-p plakous`) times
`Agent::prompt` to the authoritative result handle on a fresh agent, asserts
public pointer-identity sharing of pre-turn history into the prepared request
and committed result, and proves the configured root prompt-queue bound by
asserting `Backpressure` on the first prompt past it. `perf_project`
(`-p plakous`) times `CanonicalHistory` append, `ModelRequest` construction
and clones, synthetic `ActiveFamilyArtifact` envelopes and tool-schema
validation over the provider-neutral model API, with exact-scope accounting
on a synthetic wire body. `perf_store` (`-p plakous-store --features sqlite`;
`-- --small`, `-- --long` or `-- --dry-run`) runs the same commit trace on the
memory and SQLite backends, then a separately instrumented SQLite trace, a
process-isolated reopen with Linux RSS, and a bounded-actor saturation
fixture that requires typed `BackendBusy` backpressure. Each emits JSON Lines
(`plakous.perf-core.v5`, `plakous.perf-project.v6`, `plakous.perf-store.v6`)
whose metadata binds toolchain, target, feature closure, declared profile,
commit, dirty state and a tracked-content closure; the caller supplies
`PLAKOUS_PERF_HOST_ID`, `PLAKOUS_PERF_FEATURE_FLAGS` and
`PLAKOUS_PERF_RUSTFLAGS_ID`, because the workload never reads a hostname.
No numeric baseline is ratified: every latency, RSS, size and growth value is
a raw, host-qualified observation, and a primary timing never includes the
instrumented or isolated probes. Allocation counts are unavailable in process
because the workspace forbids an `unsafe` global allocator wrapper; instead
`tools/perf/perf_allocations.py` runs the
`--isolated-probe` entry points of `perf_core` and `perf_project` under
Linux glibc `memusage` and emits that evidence as separate JSONL.
`tools/perf/perf-check.sh --quick` (or `--full`) runs and schema-validates all
workloads into `target/perf-check.*`.

The instrumented event-history sample measures atomic attach and clone time,
retained turn count, and serialized cursor bytes before and after its measured
turn. It does not serialize or retain an L0 conversation-presentation cache;
canonical item count is a separate authoritative-history workload fact.

The opt-in hosted test
`runtime::child_projection::perf::streaming_children_and_faces_cost` emits
`plakous.perf-child-observation.v1` rows for eight streaming children with
0, 1 and 3 faces per child, 8,192 events per row and 64 text bytes per event.
Every live child reduces even with no face attached, including headless
`plakd run`; zero-face reduction is not free. One Linux x86_64 debug/test run
measured respectively 4,757, 4,879 and 5,045 ns/event, with 520,576, 520,832
and 521,344 accounted retained bytes across all eight children. These are raw
observations, not thresholds or a cross-host baseline. Timing includes source
delivery, reduction, snapshot reads and face-tail reads, not providers, stores
or terminal rendering. Retained-byte accounting covers state, inline reducer,
ring capacity, shared update allocations, text payload capacities and tail
values; allocator overhead, task stacks and process RSS are excluded. Tail
reads share Arc-backed updates; the open overlay clones the ops it emits. Run
this ignored test explicitly
with `PLAKOUS_PERF_HOST_ID` set; ordinary gates never enforce its timings.

### 2.8 Transport fault injection

With `test-support`, `PLAK_TEST_RELOAD_PROBE_LAUNCH_DELAY_MS` (an unsigned integer
in milliseconds) delays the installed-build reload probe before launching the
executable, outside the child's response budget. It models OS executable-launch
latency without changing production timeouts or probe answers. Ordinary builds
do not read it.

`plakous-http`'s `fault-injection` feature makes chosen exchanges fail the way
the network or a provider would, at the one place every provider's traffic
passes: `Client::execute`. The classifiers above the transport then see the same
`Failure` or response head they see in the field, so a fault exercises today's
kernels rather than a parallel path. A `plakd` built with `test-support` reads a
plan from `PLAK_TEST_FAULTS` once per process and applies it to every client,
against real providers or local peers alike; tests attach a plan to one client
with `Client::with_faults`.

A plan is `;`-separated rules of `,`-separated `key=value` fields. `host=`
(exact, case-insensitive) and `path=` (a prefix of path and query) select
requests; `nth=` picks the n-th request that rule matched, counting from 1, and
without it every match fires. Each rule counts its own matches, and the first
rule whose ordinal it is wins.

| `fault=` | Effect | What a classifier sees |
| --- | --- | --- |
| `400`, `401`, `429`, `500`, `502`, `503`, `529` | An in-memory HTTP/1.1 peer answers; nothing is resolved, connected or sent upstream. `retry-after=<s>` adds that header | That status, `content-type: application/json`, `x-plak-injected-fault`, and a body both error dialects read: `{"type":"error","error":{"type":…,"code":…,"message":…}}`, with `overloaded_error` for `529`, `rate_limit_error` for `429` and `authentication_error` for `401`. A token-path `400` exercises rejected refresh and credential parking without a live auth endpoint |
| `xai-refusal` | The same in-memory peer | HTTP 403, `x-plak-injected-fault: 403`, and xAI's pre-generation usage-guideline refusal body `{"code":"permission-denied","error":"I'm sorry, I can't help with that request."}` |
| `connect` | The exchange fails before resolution | `Failure(Connect, NotStarted, Io)`, as for a refused connection, but without connect-attempt counts; a real all-address refusal retains its IPv4/IPv6 attempt counts |
| `hang` | Waits `seconds=<s>` before resolution, without DNS, a socket or upstream traffic; cancellation, dropping the request future, or the exchange deadline ends the wait early | `Failure(Connect, NotStarted, DeadlineExceeded)` when the wait or deadline expires; `Cancelled` on cancellation; no connection-use fact or connect-attempt counts |
| `write-reset` | The exchange fails before resolution | `Failure(RequestWriteOrResponseHead, RequestIncomplete, Protocol)`, as for a peer that resets the connection while the request body is being written |
| `record-rejected` | The exchange fails before resolution, with no upstream traffic | `Failure(RequestWriteOrResponseHead, RequestIncomplete, Protocol)`, cause `peer_rejected_record`, TLS `AlertReceived(20)`, HTTP/1.1; all kernels label it as corruption in transit and resend immediately on a fresh connection after the first physical failure |
| `reset` | The real response passes until `after-events=<n>` SSE events (a blank line ends a block; a block of only `:` comment lines, such as a keep-alive, dispatches no event and does not count) or `after-bytes=<n>` bytes, cut exactly there | `Failure(ResponseBody, MayHaveTransmitted, Protocol)`, as for a peer that drops mid-body |
| `eof` | The same cut, then a clean end of body | The end of body a closed stream gives |
| `stall` | The same cut, then `seconds=<s>` of silence under the body's own timeouts and cancellation, then the rest | `DeadlineExceeded` if a body deadline expires first |

`stall` connects to the real endpoint and pauses its real response, even with
`after-events=0`. For an offline deadline qualification use, for example,
`host=example.invalid,path=/usage,fault=hang,seconds=30`. `hang` and status
faults such as `fault=401` are pre-connect and never contact the endpoint.

For example, `PLAK_TEST_FAULTS='path=/v1/messages,nth=1,fault=record-rejected'`
exercises the first-resend rule without uploading content. Like `write-reset`,
this synthetic pre-resolution failure has no connection-use fact; real TLS
pool tests pin preserved fresh/reused facts and eviction of the rejected
connection.

Paused-clock Responses, Messages and Chat tests use `fault=write-reset` and
`fault=record-rejected` to exhaust the sixteen-request incomplete-write
allowance across more than six virtual minutes, without real upstream traffic
or wall-clock sleeps. Responses additionally injects eight incomplete writes
followed by an HTTP 503: turn inference retains all four ordinary attempts,
whereas an unsent-only session workflow stops at that first authoritative
response. Each generation revalidates and releases its authorization; exhaustion
retains definite refusal facts rather than inventing a possible hosted effect.
Hard caller deadlines still cut the outage wait short. Existing actual TLS
and response-head regressions keep complete writes, other alerts and
post-response failures outside this allowance (chapter 01 §13.1).

`FaultPlan::parse` refuses an unknown field or fault, `nth=0`, a `hang` or `stall` without
`seconds`, and fields that do not apply to the fault. A plan from the
environment that does not parse fails every exchange before it starts with
`Failure(Resolution, NotStarted, Internal)`, which no network event produces, so
a mistyped run is never silently fault-free. The transport does not print it:
the product may hold the stderr lock, and a library write would block.

`crates/common/http/tests/faults.rs` proves each fault fires where its rule says
and equals the real event: a refused connect, a peer that closes mid-body.
The transport fault unit tests prove `hang` opens no socket and settles on
cancellation or deadline. The product's `web_endpoint.rs` console fixture
injects it into an allowance GET and retains recorded totals alongside the
per-account deadline row.
`multi_wire_loopback.rs` in `plakous-openrouter` runs each status and `connect`
through a real turn and compares it with the loopback gateway answering the
same way, and proves that `reset`, `eof` and `stall` after the first stream
event are replayed and the turn completes with the replay's output alone;
`local_transport.rs` in `plakous-anthropic` does the same for `529` and retries
a `write-reset` before acceptance. MCP servers reached over streamable HTTP
use the same transport (chapter 01 §8.5), so a plan covers their traffic too;
`tests/http.rs` in `plakous-mcp-fake-server` injects `connect` into one
`tools/call` and proves it settles as a tool failure the server never saw.
Injected statuses, `connect`, `hang` and `write-reset` never reach a connection, so
their responses and failures name no `ConnectionUse`; stream faults ride a real
connection, name it, and never return it to a pool.

The turn retry policy (spec 01 §13.1) is pinned at three levels. Unit tests in
`plakous-exchange` (`attempt/retry.rs`) cover every decision row, the budget,
`Retry-After` clamping and equal jitter through an injected sample. Loopback
tests pin each family:

- Responses, `attempt_host/tests.rs` in `plakous-responses`: a reset
  after send is replayed for a cost-only turn and never for a server-effect or
  session-workflow request; a stream cut before any tool call is replayed with
  the retry marker ahead of the replay's events; an in-band `rate_limit_exceeded`
  or `server_is_overloaded` terminal is replayed only for a cost-only request; a
  stream cut after a local tool call began discards the partial and replays; the budget ends on the
  fourth failure; cancellation during the wait sends nothing more; and the
  shared model host restarts a discarded response.
- Messages, `plakous-anthropic`: in `model/tests.rs`, a peer that reads the
  request and drops TLS without `close_notify` is replayed for an API-key turn,
  a truncated stream and an accepted stream that only refuses are replayed, and
  four 429s spend the budget with markers 2/4 to 4/4; in `oauth_model/tests.rs`, the
  same drop is replayed for a Claude OAuth turn and never for a session workflow
  (compaction); in `tests/local_transport.rs`, the drop is replayed for a
  cost-only request and never for a hosted programmatic-tool (server-effect)
  request, a reset after streamed text is replayed with its text discarded, a
  stream cut after a complete local `tool_use` block replays and only the replay's complete response executes the tool, and cancellation during
  the wait sends nothing more.
- Chat, `tests/multi_wire_loopback.rs` in `plakous-openrouter`: injected 429,
  500, 502, 503 and 529 statuses take four requests for a turn; an accepted
  disconnect, an in-band rate limit, an HTTP 5xx and a body deadline each take
  one request as a session workflow; a refused connection is retried, injected
  `reset`, `eof` and
  `stall` stream faults and an in-band gateway overload are replayed, and a
  stream cut after a raw local `tool_calls` delta is replayed with the partial discarded. Chat has no
  server-effect or cancellation-during-wait loopback; both rest on the shared
  policy unit tests.

All kernels pin terminal `x-should-retry: false` and body-classified quota or
billing failures even on 429 with Retry-After; true cannot broaden policy.
SessionWorkflow tests retain single dispatch after possible acceptance or any
HTTP response; unsent retries read typed transmission/phase evidence only.
Loopback TLS 1.3 peers emit authenticated alert 20 both mid-upload and after
the whole body but before the head. Transport fixtures pin `RequestIncomplete`
with cause `peer_rejected_record`; the Messages `SessionWorkflow` path retries
once and succeeds under the existing budget. Alert 20 after a complete head
does not retry that workflow. A partial head already rules out this proof;
transport fixtures pin that first-byte boundary for separately delivered and
co-buffered plaintext plus alert, a non-20 pre-head alert, and unchanged TLS 1.2
and HTTP/2 classification. The proof relies on rustls 0.23 deferring TLS 1.3
KeyUpdate replies until the next outgoing message; TLS 1.2's possible
post-request `no_renegotiation` reply excludes it. These are physical TLS
fixtures, not injected
status faults or classifier strings; local decrypt errors remain distinct.
`plakous-http`'s `tls_integrity` tests pin that the client's own write path
never produces alert 20: a loopback Rustls peer with a small receive window
drains 100 KiB to 3 MiB bodies in odd-sized reads, bursts or stalls,
authenticates every record and checks every body byte against a
position- and request-dependent pattern, on unpooled, reused pooled and
concurrent pooled connections, with a copying observer, with peer TLS 1.3
KeyUpdates mid-body and while idle in the pool, and around uploads cancelled,
dropped or deadlined mid-write and then retried. A positive control flips one
ciphertext bit and requires the peer's `DecryptError` and the writer's
`AlertReceived(BadRecordMac)`. The ignored
`tls_integrity_reproducer_matrix` widens this to every size, pace, key-update
and observer combination plus TLS 1.2 (run with `--run-ignored only`; its
nextest override allows 300-second periods).
Pool fixtures pin the five-second idle bound and content-free checkout age in
failure diagnostics. Product telemetry tests pin replay-loss counts, ids,
`v_replay_loss`, retention and disposable schema version 21.
Responses in-band rate-limit and overload fixtures pin failed lifecycle,
closed observations, fresh terminal facts and one physical request for session
workflows (including compaction) and turns carrying `x-should-retry: false`.

Product tests pin the `plakd run` retry line, the JSONL `retry` frame and
(in `plakous-hosted`)
that a discarded response's sealed reasoning never reaches the session. `check_features.py` requires that a default `plakd`
never enables the feature and that `test-support` does.

A host plan may set `hop=gateway` or `hop=upstream`. An absent hop means
`upstream`. `hop=upstream` applies to the provider client; in a plakd deployment
that client is in the separate gateway service. The live drivers put upstream
rules on their fixture gateway and relay-hop rules on the caller.
`FaultPlan::parse_for` with `FaultPlacement::ExternalGateway` refuses an
upstream rule, so a plan that cannot reach a provider from this process is
never silently fault-free. `PLAK_TEST_GATEWAY` selects that placement: unset
or `in-process` is local (used by library fixtures), `external` refuses upstream rules, and any other
value fails every exchange the way a mistyped plan does. `provider` is not a
placement. The separate provider-plan parser, `FaultPlan::parse_provider`,
accepts no `hop`, `relay-lost`, `gateway-refusal`, `retry-after-ms`,
`credential-status` or `supported`.

`hop=gateway` matches the host and path of the exchange it is applied to.
A relaying client applies its plan's hop-1 rules to every relay request it
sends (each re-preparation counts), matched on the logical upstream host and
path, through the same composition a real hop event takes: `connect`,
`hang`, `write-reset` and `record-rejected` fail the hop before the gateway
holds the whole request (upstream `NotStarted`); a status fault or
`gateway-refusal` answers in the gateway's place without sending the request;
`relay-lost=before-head` sends the request and loses the answer
(`MayHaveTransmitted`); `relay-lost=after-head` drops the hop right after
the relayed head (a `ResponseBody` break); `reset`, `eof` and `stall` apply
to the relayed body. The dialed hop itself is excluded from selection. A
direct client placed on that hop with `Client::with_fault_hop(Gateway)`
applies hop-1 rules to its own exchange.
Hop-2 rules are selected by the gateway's upstream client. Rules on the other
hop are not counted. On hop 1 the existing faults keep their meaning:
`connect` and `hang` are gateway unreachable (`NotStarted`); `write-reset`
is an incomplete hop-1 write, which the relay map turns into upstream
`NotStarted`; `reset`, `eof` and `stall` cut the hop-1 body; a status fault
is a synthetic head and does not open a socket.

`relay-lost=before-head` and `relay-lost=after-head` require `hop=gateway`.
Before head the caller sees `Failure(ResponseHead, MayHaveTransmitted, Io)`.
After head a relay `Head` frame is delivered and the body then breaks.
`gateway-refusal=<code>` answers with that refusal's status,
`plak-gateway-error` and the protocol refusal body, without a socket.
`fence_moved` is not a status injection; the fixture gateway scripts it,
because the refusal carries account facts. `retry-after-ms` belongs to
`busy`, `credential-status` to `credential_not_ready`, and `supported` to
`version_unsupported` (`+`-separated, default `1`).

`RelayClient` composes hop 1 onto the kernels; no separate model of that
composition exists. Unreachable,
write-reset, `busy` and `fence_moved` are unsent `NotStarted` and refunded;
`busy` also spends the unreachable budget, and `fence_moved` re-prepares
inside the attempt. Other refusals, including `internal`, are unsent and not
retried. A loss, cancellation or gateway failure before head is
`MayHaveTransmitted`. `End` `upstream_failed` before head is the measured
hop-2 fact. A break after head is the existing mid-stream failure. Trailers
then complete, or complete, is a clean end. The incomplete-write allowance
does not apply to hop 1.

### 2.9 Credential gateway evidence

`plakous-test-support`'s `gateway` module is a scripted
`GatewayService` on `plakous_http::relay_server::RelayServer`, which is the
only protocol implementation. Loopback upstreams are reached through
`RelayServer::with_dial_override` (`fault-injection`, repository-only); pin
checks still see the logical URL. The service can refuse, drop before a
response or after `Head`, stall, restart onto a new instance id, and answer
`fence_moved` on a chosen relay. The scripted credential is an API key.
Subscription refresh is not scripted. The service admits any upstream URL
under its logical origin rather than a provider's pin rows, so its relays are
transport evidence, not pin-enforcement evidence; each provider's
`upstream_pins` tests and the custody tests over the real pin tables own pin
enforcement.

The real `plakd gateway serve` process also runs against an offline OAuth
peer. Under `fault-injection`, `PLAK_TEST_GATEWAY_OAUTH_PORT` routes only the
pinned Codex device-code POSTs and the Claude token POST to that loopback
port; other exchanges use the test's fault plan. The process tests cover a
Codex device challenge and polling, refusal of a duplicate start before
credential retirement, and a TCP admin being refused replacement of a
Ready Claude record without changing the record. Concurrent admin calls
and Drain are exercised with a held provider request; the holder lock
remains exclusive until that request and the service settle. A separate
worker test checks that a panicking call is refused and Status still works.
The shutdown timeout test confirms that the holder remains exclusive even
after the detached worker returns. CLI and console tests cover per-account
row errors and the diagnostic on a failed account list.
Startup tests check that the gateway advertises only once listening, and
that host startup waits through a missing holder, unpublished or missing
socket, and a socket that is bound but not listening.

`PLAK_TEST_GATEWAY_API_PORT` reroutes only the pinned OpenAI Responses
POST to a loopback peer in the same fault-injection builds. Unlike the
OAuth fixture, it applies status and stream faults to the logical endpoint
before dialing, so the deployment canary test exercises completed output,
mid-body reset and deadline through the real gateway and counts physical
requests at the peer.

Relayed-path parity captures the upstream HTTP/1.1 request at that loopback
peer. Those bytes, with the dial authority normalized, are pinned equal to a
direct client exchange to the same peer, with the custodied value substituted
in place and the header order unchanged.

The durability profile holds the API-key crash rows
(`test(gateway::durability::)`) and the subscription refresh crash cuts
(`test(subscriptions::custody::tests::durability::)`); the offline lane
excludes both. A gateway killed after `Head` ends the body as
`MayHaveTransmitted` and does not replay. A host killed mid-stream, and a
caller drop, each cancel the upstream within one second and leave one dial. A
restarted gateway serves the next request on a new instance id.

The refresh cuts run a gateway `SubscriptionCustodian`'s forced Claude refresh
on the gateway's own runtime and shut that runtime down at the cut; a dropped
caller is not a crash, because a forced refresh runs detached from it. A
fault-injected token client places the cut: `hang` before the grant is sent,
`stall` after the provider answered and rotated it, and none for the cut after
the store commit. A gateway restarted over the same store answers the remote
side through `GatewayCustody`. Both pre-commit cuts leave
`RefreshOutcomeUnknown` at the claim's generation. The remote side must then
see quarantined facts and a `credential_not_ready` refusal of a relayed
request without a dial, and the operator's forced refresh sends no grant,
because Claude never reclaims. The post-commit cut is Ready at the generation
after the claim, and a request prepared on the seeded generation is
re-prepared onto it and carries the rotated token.

`just live gateway` prints the plan and a `just live estimate` line, and
calls no provider. `just live gateway --paid` drives the shared live route
drivers through a separate `plakd gateway serve` process holding copied keys
and its test subscription store. Paid runs on the production machine use a
separate `XDG_STATE_HOME` with its own login state, because the fixture
gateway would otherwise contend for the production gateway's exclusive
holder lock. A keyless caller sets `[gateway] address`
to the service's Unix socket. The run drives one turn on each of seven routes (`codex`,
`claude-haiku55-low`, `grok`, `api-openai`, `api-anthropic`, `api-xai`,
`or-nemotron-free`), restarts its gateway, reopens every session and forces a
refresh of each OAuth kind (codex, claude, grok). The Unix-socket run checks
that `plakd doctor` reports each OAuth row OK and Ready from the gateway,
and that `plakd status <account>` reports each credential Ready. Each
Unix-socket turn and reopen records the gateway audit line count. A binary
without `plakd gateway serve` fails as a harness error before any provider call. Tailnet TCP from a
second machine, remote login, the kill switch and a `hop=gateway` fault run
are not driven. The estimate line covers the API-key turns; subscription
routes stay quota. Results are recorded in `tests/live/STATUS.md`.

## 3. Evidence classes

The Changes executor's offline fixtures exercise stale worktree/index inputs,
fixed HEAD comparison, binary/untracked/deleted/unborn cases, literal containing
repository pathspecs, hostile filters and Git helpers, protected administrative
directories, oversized inputs/output and entry counts. Serial process-stop
fixtures verify byte-cap, deadline, cancellation and dropped-future group
cleanup, including descendants retaining pipes. These tests never invoke `jj`
or a provider, and production probes never write an object into the user's repo.
Paired-runner conformance compares the exact local/remote status, text/binary
diff and stale-index refusal through a placed execution environment, and
verifies that a home-local process transport neither receives nor serves a
Changes request. Missing-promisor-object fixtures include a nonblocking local
listener and hostile per-protocol policy, proving no network connection or
object write. Unsupported version, ignored attribute-source and disabled-CLI
fixtures prove closed `vcs_unavailable` refusals. Empty and relative PATH marker
fixtures prove that repository executables never run. Staged-then-restored
worktree fixtures assert both index bytes and inode are unchanged, including
hostile repository settings. Local-file promisor fixtures test the protocol
allowlist and no-lazy-fetch independently, with a positive fetch control.
Ownership mismatch, metadata symlinks and alternates fixtures pin repository
admission, and an unattributed default textconv marker pins `--no-textconv`.
Maximum-field 2,000-entry status fixtures pass both local validation and the
runner codec/frame. A status exactly one byte over the internal 12 MiB budget
is refused by both, even while fitting the runner's larger frame ceiling.

Two classes of evidence exist. Each answers a different question, and neither
substitutes for the other. Offline tests are never a substitute for live
protocol or model behavior, and live observation is never a substitute for an
offline contract. §3.4 is the pre-release bar for those live runs. It does
not move library evidence out of §3.1.

### 3.1 Offline fixture evidence

Slow-link fixtures cover atomic client writer byte/frame reservations,
withdrawal of an unwritten draft at the answer deadline followed by pipe
drain and a single submission, and explicit same-id checks preserving
`duplicate:true`/`draft_pending` with generation fencing. Paused-clock
throttled pipes and a real WebSocket codec cover multi-minute large
baselines, an unaffected healthy second writer, no-progress detachment and
the hard total deadline despite continuing trickle progress. These tests
use no live host or network service. Withdrawal and idle-deadline renewal
are mutation targets; elapsed simulated time is not live bandwidth evidence.
Review regressions cover reserved control headroom and exact control
replay on overflow (detach before attach, transcript open on the current
attachment), queued-attach retry/error clearing and waiter notification,
FIFO suffix withdrawal, a mid-write unknown
outcome retaining draft identity while a later line survives, and single
signalling of check saturation. Paused-clock tests drive the actual web
attachment loop through a throttled baseline with an outstanding ping,
including Pong/Bye buffered during the send ahead of a ready heartbeat
tick. They also cover the web outbox byte cap/reset and stop in the actual
web `send_pending` path, as well as the socket path. An attached extension
intent counts serializer invocations to forbid a redundant size-only pass. Control
headroom, pong re-arming and FIFO suffix withdrawal are mutation targets.
Exact-control replay, retry-state clearing, overflow notification and
buffered-Pong priority also have targeted mutation witnesses.

`product_runtime_e2e_tests` covers runtime behavior: exact-resume
publication, cross-profile history/queue and reverse switching, fast-lane and
native-PTC/direct transitions, independent fork trees, refused/already-active
forks, treeless-source seeding and stale-pause follow-up draining. Their
durable/dispatch assertions are retained, not replaced by render assertions.
They run in the serial group. Shared host/catalog fixtures remain
test-only.

`product_runtime_e2e_tests::exited_child_output_over_8192_commits_and_reopens_without_recovery_required`
drives the actual process backend through start and poll, checks both successful
tool rows and the answer, then opens the exact runtime locator with no extra
provider dispatch.
`product_compaction_runtime_e2e_tests::exact_overflow_compacts_reopens_and_continues_without_terminal`
uses exact projected-input calibration.
It checks current Fits versus pending WouldOverflow, scheduled workflow settlement,
publication boundaries and distinct locators, retained causal history, exact reopen
and subsequent input. Its physical dispatch counts are two, four and six; no
effect dispatch occurs. It runs in the serial lane.

`observation_parity_tests::abandoned_open::abandoned_recovery_reaches_host_and_jsonl_once_without_persisting_a_duplicate`
uses a fenced-open fixture with and without
a durable submission. Host and JSONL receive one recovery row; no duplicate
is persisted.
`offline_e2e::projection_parity::real_jsonl_rows_equal_exact_resume_rebuilt_from_deleted_views_without_redispatch`
runs six view-parity scripts, removes the saved views,
and compares exact reopened rows and complete transcript with unchanged
dispatch evidence.
`offline_e2e::reopen_probe::a_headless_reopen_probe_leaves_the_live_tree_unchanged_and_its_owner_keeps_saving`
uses the original two-tree-turn script: read-only reopen probes neither take
the live owner's lease nor change its revision/generation, and its next turn
still saves successfully.
`product_crash_tests::carry_law` exercises binding-repair carry and persisted
orchestration-intent settlement cuts, relaunching to one successor at the line
head, including its ignored process-child fixture.

`host_skeleton::bridge` uses the real public stdio bridge and host. Its single
client-originated handshake reaches the server unchanged, including version
refusals; EOF leaves a busy root alive with one physical provider dispatch,
and a new attachment reaches the same root. A same-user bridge still refuses
local-only reload intents before model work. The CLI contract
`a_stdio_bridge_without_a_host_does_not_create_a_home_or_embed` proves absent
host discovery leaves protocol stdout and the product home untouched. A build
without `web` qualifies the corresponding typed capability refusal separately.

The opt-in starter's real bridge fixtures prove that caller process-group
signals do not reach its detached host, concurrent bridge/direct starters
reach one host generation, and a held other key refuses without creating an
auto-start log or sibling. Remote-shell and installed-unit cases refuse before
startup. Caller-only and absent API keys fail in the auto-started resolver,
while a target-only key admits through an existing host without sending any
model prompt; diagnostics identify the resolver and never include values.
Bounded-log tests also refuse non-private files and symlink targets without
modifying them. `plakous-runner`'s
`a_home_started_service_has_no_implicit_workspace_and_keeps_the_home_protected`
keeps service discovery distinct from explicit workspace admission.

`an_auto_started_host_exits_after_its_idle_window_but_a_foreground_host_persists`
is a serial wall-clock fixture: it runs both hosts with a five-second window
through the `test-support` override `PLAK_TEST_HOST_IDLE_EXIT_SECONDS` (a whole
second from 1 through 300; anything else keeps 300 s, and other builds do not
read it), waits that window for real, checks ordinary host persistence beside
it, and owns both hosts' cleanup. A detached host start forwards the override;
`tools/gate/test-env.sh` unsets it. The pure idle-clock test keeps the
production five-minute window: it builds the clock through the production
constructor and resets the full window on activity or kept work. Idle eligibility includes residents,
opening/releasing workers, startup recovery and durable runner-stop debt; an
unreadable directory is not proof of idleness. No test achieves idle exit by
evicting a session.
`idle_exit_keeps_a_dormant_continuation_without_opening_it` also prevents an
unsuccessful startup open from making durable handoff debt look idle; checking
eligibility neither opens the session nor evicts work, and clearing the durable
continuation makes the dormant session idle-eligible again.

`wire_transport::hosts::tests` distinguishes held kernel locks from stale pid
text, discovers every live socket key, and refuses unsafe locks, directories
and state-directory links. `host_skeleton::singleton` drives the real host:
an older key held without a socket refuses before any session starts, a held
home guard is not absence, and a free older-key lock naming a live pid does
not block startup. The same-key second-host/restart test retains its process
and socket-cleanup assertions. `host::lock::tests` also proves a linked lock
cannot truncate its target.

Bare help and removed terminal-launch flags are real-binary contracts: help
succeeds without home/runtime creation, while removed flags are usage errors.
Secret-input PTY/termios tests remain because CLI authentication is interactive.

`host_skeleton::reload::a_host_refuses_requested_reload_without_any_client_or_redispatch`
uses the real host and socket with idle and busy roots. After the only client
disconnects, the host's pid owns the announcement and the service records a
refusal; a fresh attachment reaches the same session with one physical
provider dispatch. Neither client polling nor a terminal is required for the
public runtime invariant.

Settings payload and publication fixtures use product fields. Client-local
preferences are not serialized or persisted by the host. Role checks refuse
remote legacy saves before product dispatch; confirmed operator paths keep
their capability, consent and etag rules (current client protocol 12).
Product DTO contract examples decode and re-encode unchanged before and after
extraction, including temporary reload values. Schema extraction is checked
against raw `plakd wire schema` output with an uninstalled executable and an
empty identity cache, so both captures consistently report no binary identity.

Public control successors derive their action fixtures from complete real-client
traffic and replay them through the actual `plakd host connect --stdio`
executable. Qualification compares each direction's exact control sequence,
not a byte-identical duplex schedule. Directory, attach, new, prompt, stop,
bye and their acknowledgements, other pinned controls, connection lifetimes
and semantic barriers remain exact, with generated identity aliases preserving
correlations. Independent opposite-direction frames may interleave differently;
each response must still follow its own request, and attachment-scoped controls
must follow their attachment grant and precede detach or client close. Connection
opens and semantic barriers fence both sequences; EOF fences its own direction,
allowing the receiver to drain replies after a write half-close. Missing,
duplicate, extra or within-direction-reordered controls fail. No frames are
sorted or additionally filtered. Identity alias allocation uses direction order
so independent duplex interleaving cannot change an alias.
The socket replay checks directory rows against the live
`updated_at DESC, session ASC` order, then compares every complete row by its
bound session identity. Capture timestamps are opaque aliases, so they cannot
freeze row positions across runs: a current-second tie legitimately uses the
current UUID order. This does not reorder frames, omit rows or fields, or hide
changed titles, activity or placement. Wrong live ordering and missing or
changed rows have explicit refusal witnesses.
Runner-hold budget evidence enters the unsent hold before advancing a paused
Tokio clock and requires the exact budget shift; task scheduling and early
timer wakeups are not held time. The unreaped-leader fixture waits for observed
exit, then requires a complete process scan to report zero. A bounded scanner
timeout is unknown, not a positive zombie count: only unknown scans are retried
within the existing fixture bound, and a confirmed nonzero count still fails.
`client_traces.py --check` compares normalized directional controls and checks
the complete reads audit against that capture's own raw recording. Audit UUIDs,
legal-read pairs and SHA provenance are capture-specific, not cross-capture
stability evidence. `--check-controls` compares only normalized directional
controls across rounds or clients, while still validating each raw recording's
legal-read exclusions and control causality. The fixture retains the original
duplex entries as a replay stimulus, not as a unique cross-direction schedule.

Hello binary identities compare as `<client-binary:N>` and welcome identities
as `<host-binary:N>`, numbered by first appearance independently within each
role. `unknown` is an ordinary value for this purpose. Repeats correlate and
distinct values never merge within a role; cross-role equality is not compared.
The audit, outside the compared control stream, retains a global table of raw
binary values, their roles, aliases and original connection numbers, together
with the capture's SHA provenance. Raw traffic remains unchanged.
Only a completely frame-free connection with one open and both directional
EOFs is excluded as a discovery probe, with every exclusion retained in the
audit. An incomplete connection, a reset or any frame keeps that connection
in the comparison. Retained connection numbers are aliased by real connection
order, never merged or sorted. The public capture driver's `ready` and
`reconnected-without-replay` barriers observe the actual initial topology and
promptable accepted acknowledgements on that connection before marking ready;
they do not schedule emissions or reorder frames. Negative tooling tests pin
identity separation, nonempty probe retention, every required-control rejection
class, attachment lifetimes and semantic boundaries; a legal independent
cross-direction interleaving passes.

The native-client immediate-exit regression repeats 200 real subprocess
connections, alternating implicit drop and explicit goodbye before drop. Each
child queues a control frame, drops both handles and exits immediately; the
peer must receive that frame followed by exactly one `bye`. A serial gated
writer test pins the two-second last-handle drop bound without changing
unwritten/started-write dispositions. Captures require `bye` on every real
connection, including reconnects.

Only three explicitly named read-only requests and their acknowledgements may
interleave: `refresh_composer_chrome` (timer-driven chrome), `resync_transcript`
(resync at a reconnect sequence boundary), and `list_agent_acknowledgements`
(an acknowledgements read). Each complete request and its unique well-formed
answer is validated in retained complete raw traffic. Trace
validation rejects a legal read after that connection's bye or on an attachment
not returned by a preceding attach on the same connection. Separate real-bridge
assertions exercise every recorded legal read with unchanged session, queue,
transcript and tree state and no provider dispatch. Every journey ends with
exact equality between the face transcript and its durable view: resync must
not conceal missing or duplicated content. Any other unpinned interleaving
fails; adding to the legal-read set is a reviewed change. No scheduling,
emission control or unbounded capture-variant collection replaces this rule.
Typed face-transcript equality covers row content, order and multiplicity on
both connections; live provisional row keys need not equal committed anchors.
`wire_host_restart` and `wire_host_restart_switch` cover root and child twin
pilots and every reached per-store commit cut. Their indexed signatures match
the original terminal pilots, including labels and their per-store order; a run
of consecutive unlabelled commits in one store compares as one run, because how
many commits the projector takes for one stream of rows depends on timing, not
on the face that drove it.
Recovery reconnects without replaying input and checks durable stop, custody,
no handoff/continuation and no extra root or child provider dispatch. These
matrices are in the durability lane with the reviewed longer timeout; the
original terminal matrices remain separate evidence.

Each of these four restart families exposes eight root groups and eight child
groups as independent durability tests (`root_0` through `root_7`, and
`child_0` through `child_7`). They partition the complete sorted live pilot
inventory, not a pinned subset: each test asserts the partition has no missing
or duplicate cuts, and the final completed group checks the union of actual
per-group cut manifests. Per-cut assertions and twin-pilot signatures are
unchanged. A file lock shares each family/child twin pilot once per nextest
run, under that run's isolated temporary directory, keyed to the exact test
and product executables and fixture inputs. A pilot panic publishes its
reason for the dependent groups, which report blocked cuts rather than
retrying or passing without evidence. A builder that exits before publishing
a result also blocks the dependents. `NEXTEST_RUN_ID` is shared only
when `NEXTEST=1`; Cargo test and crash-sweep invocations use a process ID and
start-time identity even if an earlier run ID was inherited. No pilot survives
across runs or checkouts. All sixteen tests of a family must run in the same nextest
invocation on one host: splitting a family across invocations, partitions,
lanes or hosts loses the shared-pilot and complete-union evidence. This is a
coverage requirement, not an isolation requirement; the cases run concurrently.
The strict sweep selects these families by prefix, runs the complete selection
with `--test-threads <jobs>`, and refuses an
incomplete or empty group listing rather than succeeding with zero cuts.
In wire replay, the `stopped` barrier waits for the durable stop notice
before sending bye, in both pilots and cuts. Traced runs also wait for the
complete indexed product notice write after the stop request: a reader can
see the durable notice before the WAL callback publishes its trace line.
A killed host ends the cut's wait. Both twins report indexed table writes
when they diverge. Twin signatures and every reached per-store prefix
remain exact; only the cross-face parity comparison merges runs.

Protocol-4 attention is tested while busy; clients keep queueing with
`follow_up`. Child counts exercise live, parked, completed and held-loss
transitions. Dormant stops are replayed and reopened under `continue`,
with unchanged provider-dispatch evidence.

| Selection and carry contract | Offline evidence |
| --- | --- |
| v12 selection and committed named-state carry | `v12_document_round_trips_named_state_selection_and_full_history`; the real SDK driver exports committed values; `oversized_named_state_is_refused_before_import` |
| v12 imports with named state; v11 refused | `v12_import_preserves_named_state_and_v11_is_refused`; `v8_is_refused_before_import_regardless_of_named_state_or_selection` |
| v13 kind, runner and profile; malformed v13 refused | `v13_names_its_kind_runner_and_profile_and_round_trips`, `malformed_v13_documents_are_refused` |
| A frozen v12 document imports with its saved runner and profile with an identical history | `a_frozen_v12_document_imports_with_an_identical_canonical_history`, `a_v12_session_imports_and_exports_as_v13_with_an_identical_history` |
| Typed rows, send times and process commands for v13 and v12 | `current_and_previous_documents_preserve_every_transcript_row_type`, `current_and_previous_documents_carry_send_times_and_process_commands` |
| Import rejects pseudo-work and rebuilds durable intents | `current_and_previous_documents_reject_resolved_count_pseudo_work`, `import_intents_survive_reopen_and_rebuild` |
| Root-only selection and cross-field refusal | `sdk_is_a_root_override_not_a_child_template_capability`, `root_composition_cross_field_rules_apply_on_load_and_save`, `applying_claude_profile_discards_saved_sdk_selection`, `sdk_child_selection_refusals_are_exact_and_create_nothing`, `sdk_spawned_api_binding_refuses_before_discovery_or_creation` |
| Optional tool/MCP and v8 version refusal before creation or dispatch | `sdk_optional_tools_and_fence_refuse_before_surface_compilation`, `agent_sdk_hello_and_configured_mcp_refusal_are_offline`, `v8_exports_refuse_file_import_without_creating_sessions` |

The typed Python SDK's real-worker driver fixture exercises gathered typed
reads (without asserting execution overlap), listing, grep, plan updates,
transactional `edit_files` and the read-revision/CAS round trip, named-state
round trip and failed-cell rollback, per-result and per-cell encoded quotas,
put-time state quota errors, literal argv, shell execution, cancellation,
non-TTY stdin keep-open, and more than 64 processes with a bound jobs root
including a job-output reference read. A type-error cell must run no file
operation, and a forged `MemoryError` message remains an ordinary failure.
The serial full-driver foreground-budget test requires a typed stop;
the session-level test also requires a joined worker.
SDK stub/bootstrap hashes and generated-validator keyword coverage are pinned;
the SDK descriptor identity is pinned beside the existing goldens. Existing
text pins are unchanged; descriptors naming the revised cell protocol rotate
with the runtime format epoch.

Harbor's opt-in `plak-py-ptc`/`plak-py-sdk` A/B recipe declares composition
per arm in a digest-bound campaign. Default arms do not admit overrides;
observed composition must equal the declared value. plakd trials upload,
digest-pin and verify both cell sidecars with the binary. The adapter's
`rung-a-py.yaml` recipe fixes the eight-task slice, tuple, arms and three
repeat blocks; private inventory generation records its digest and checks
calibrations against the declared tuple. The adapter's
zero-inference suite exercises this path; neither calibration nor paid
trials belong to a gate, and adding support does not authorize a trial.

Child-observation regressions exercise the eight-subscription activity bound,
live counter replacement and terminal clearing, canonical prefix before the
addressed live tail, explicit missing-activity markers, transcript open/close,
and child cancellation/result settlement with an unread saturated host face.
Agent-transcript regressions pin agent-id identity across durable and
progress-only tree revisions. The protocol pins child-live omission from JSONL v1;
child activity must never project as root narrative or usage.

Hosted-retrieval side requests are pinned offline in the Messages kernel:
exact request-body goldens with their cost-only classification (and the
server-effect refusal of any other tool shape), synthetic SSE fixtures for
search and fetch results, citations, `pause_turn` continuation and its
three-continuation bound, `max_uses_exceeded`, empty sources and refused
blocks, and loopback retries (a connect failure and a 429 retried, a 400 not).
The Chat kernel pins the same for OpenRouter web search: exact body goldens
with and without domain filters, reasoning and retention, the one-plugin
cost-only allowlist, streamed and non-streamed `url_citation` fixtures (sources
present, duplicated and empty), refused tool calls and foreign annotations,
loopback retries, and that a main-exchange stream refuses annotations and a
main body with an enabled plugin stays a server effect; the OpenRouter
executor pins its headers, credential path and revalidation before each
physical request.
The Responses kernel pins the same for its side requests: OpenAI and xAI
body goldens (web search, X search) with their allowlist, synthetic streams
with sources, empty sources, a failed call, the captured xAI X-tool items, refused
items and events, a cut stream and `response.failed` (server error retried,
anything else not), and the same loopback retries; the OpenAI and xAI API
executors pin their side request's path, key, project and model over loopback
TLS, and a revoked xAI key refuses before the socket. The Codex
`alpha/search` body, allowlist and `results[]` mapping are pinned offline. The
Responses side-request tests pin that a transport opting in retries the Codex
wrapped-503 401 as a 503 without a claimed refresh, on both the JSON retrieval
and image policies, and that every other 401 body and a transport without the
opt-in still take the claimed-refresh path; an ordinary image 503 is still not
retried. They also pin that a transport-claimed contract refusal on retrieval
and image requests (HTTP 426, 400, 429 and 503) ends after one physical request
with its remedy text, its status and no claimed refresh, while an unclaimed
refusal keeps the provider's bounded detail, a 401 is never offered to the
transport and still takes the claimed refresh, and a 403 stays a credential
refusal. SuperGrok search and image requests pin the Grok 426 version gate over
loopback TLS, and a Claude OAuth web search pins the 400 sent-beta rejection,
each with the newer-build remedy and no provider update advice. Their
negatives are pinned too: a SuperGrok 426 with another body keeps the generic
detail, a SuperGrok 401 still runs one claimed refresh, a Claude OAuth 400
naming an unsent beta keeps the generic detail, and the xAI and Anthropic API
routes claim neither shape. The
SuperGrok and Codex executors pin over loopback TLS their path, OAuth headers,
body and one authorized inference permit (acquired before its dispatch
authorization), and that a logged-out or changed login refuses before the
socket without a permit; the kernel pins that a route's guard learns of a
connect failure and is released at the response head, and the bounded JSON
side request (a rate limit retried, an unclassified body refused before
dispatch, an oversized document refused).
The wrapper pack pins its descriptions, argument validation and result
rendering; binding pins that a `Supported` row with a port binds the pack and
every shipped row refuses. Live evidence for each (route, tool) is the
`web_search`, `web_fetch` and `x_search` feature scenarios (§3.2), and for `image_gen` the `image_gen_roundtrip`
scenario; no crate carries a paid probe.

API-route contracts run offline against the production API binding, with
fixture keys and no dispatch: the OpenAI and xAI API configured tool
definitions (names, schemas, descriptions) equal the Codex and Grok OAuth
packs' for the same composition in `direct`, `native-ptc`, `js-ptc` and
`py-ptc` (the presentation minus the costume); the OpenAI API enrollment
table's `CodeModeOnly` rows equal the Codex reference catalog fixture's, the
fixture's client version equals the pinned constant, every `CodeModeOnly`
entry is an enrolled row and every enrolled row is in the fixture or on an
explicit API-only Direct list; an OpenAI API `CodeModeOnly` row constructs,
creates and reopens a durable native-ptc session (both `plakous-default` and
explicit), while `native-ptc` on the Anthropic or xAI API fails construction
and native-ptc without the JavaScript host fails naming the sidecar; and
command parse, template load, the root rebind, `plakd run`, `plakd agent`, a
seeded hosted session, `plakd models`, `plakd doctor` and an API child's pin admit
or refuse `native-ptc` by the same table, with `run` and `agent` saving the
same record for the same arguments. One local-TLS wire contract dispatches an
OpenAI API native-ptc turn: the request carries only the `exec` custom tool
(Lark grammar) and the `wait` function, `parallel_tool_calls=false` and
`store=false`, and no Codex OAuth client headers. A session created under a
pre-pack OpenAI API presentation fails closed at reopen (harness
revalidation), as does a Grok session whose recorded projection presents
`wait_agent` or `interrupt_agent`. The rendered-prompt matrix carries the API routes as
single-agent rows through the production API profile and Platform wire.

One-shot parity invokes the production hosted `OneShot` entrypoint; it does
not mirror the CLI with a second turn loop. Gated text fixtures distinguish a
streamed prefix from canonical terminal text, including divergent previews and
completed results with no visible text. Real-path regressions cover canonical
stdout and durable transcript publication, completion winning a refused quit
cancellation, interrupted partial output, retained paused queues, a canonical
SQLite release failure after `AssistantDone`, owned child release refusal after
completion with successful parent release and exact locator publication, combined
parent/child release refusal retaining both failures, and failed product publication
followed by refusal of the stale exact locator on reopen.

Observation-parity goldens retain the host's prompt admission and delivery
receipts, including the queue identity, revision, and prompt text after the
existing identity scrub. These receipts are in-process provenance even though
the JSONL projection does not emit them as standalone wire records. A receipt
change must be reviewed in the goldens; normalization must not hide it.
Presentation snapshots and patches are validated and folded through the typed
transcript reducer before comparison. The oracle compares complete ordered keys
and row values, including all their existing identity/text fields after the established
scrub, rather than the schema/key/revision encoding of a snapshot or delta.
Patch contract tests separately pin schema, ordering, exact revision, scope and
atomic bounds refusals. A row-content change still fails observation parity;
the JSONL kind table remains checked against the production closed projection.
An assistant-completion or status boundary alone does not identify a live twin:
patches carrying canonical settlement rows at those boundaries remain compared.
Hosted coverage also folds the actual live publication into a keyed state,
checks a quiet settlement is an empty advancing patch rather than a snapshot,
and compares the keys and complete rows with an exact reopened attachment.
Usage parity omits the live request's Coding/Agent tool-inventory
informational rows and the timestamp-dependent `Live turn growth (est.)`
category: prepared-request and terminal refreshes supersede one
another, so row presence depends on mailbox drainage. The prepared-request
usage tests pin the exact tool-name split; other informational rows remain
part of the parity comparison.
The turn wait also recognizes the explicit start receipt: a fast completion
may coalesce away the busy=true edge. A real-channel regression pins that
wait behavior without relaxing the recorded observation comparison.
Queue and busy observations are latest-state snapshots: the mailbox can
supersede an unread value even across a prompt receipt or tool line. The parity
oracle compares their exact final values, not intermediate snapshot counts;
all prompt admission/delivery receipts and canonical turn terminals remain
ordered and exact. Workflow waits use their own correlated intent-start
receipt rather than requiring a coalescible busy=true edge.

Native Codex login stall tests first observe the token, JWKS or UserInfo request,
then drive its propagated login deadline with virtual time. Their setup budget
is shorter than the transport's own cap, so an omitted login deadline still
fails. The pooled protocol-terminal close test holds the body unfinished and
requires zero virtual-time delay, excluding scheduler latency from owned-task
joins. Child terminal-retirement fixtures gate provider dispatch until they
have captured the active turn control, rather than assuming a fast failed turn
is still active when start returns.

An actual interactive host test injects failed completion publication and paused-state
writes, restores storage, and proves both a manual prompt and queue drain remain
refused while settings, an explicit new session, and quit work.
The busy-follow-up ENOSPC regression additionally fails the root definitively,
retains the exact previously committed queue identity, rejects only the unwritten
draft, delivers a new prompt separately, and explicitly resumes the old item.
It checks exact delivered texts, an empty durable queue and the final empty
queue snapshot, excluding both prompt merging and stale queued presentation.
Additional definite-failure and outcome-unknown variants keep ENOSPC armed
through terminal publication, assert the refused projection fences admission,
then recover on a distinct new prompt. Both the durable queue and its snapshot
retain the same item identity until explicit resume delivers that item alone.

The same hosted entrypoints run with extension-free `Core` and an independent
extension whose marker is neither `Copy` nor `Default`. Real one-shot and
interactive tests cover immutable idle/busy and attached-one-shot facts, usage
publication before independent extension dispatch even in an idle host configured
for one-shot execution, explicit `NoHandler`
refusal without losing the admitted turn, and bounded replacement of extension
observations. The generic interrupt tests retain deferred FIFO work and never
turn a queued quit into cancellation of an admitted paid operation. plakd's
settings dispatch matrix remains binary-owned evidence.

Deterministic, hermetic, credential-free, and reproducible on any supported
host. It covers kernel grammar and framing, canonical history, checkpoint and
recovery semantics, store schema acceptance and rejection, tool contracts,
compile inventories, config parsing, prompt digests, and every
fail-closed refusal path. This is the only class that gates a change.

Fail-closed rows are coverage, not gaps: a test asserting that an unqualified
tuple refuses with an exact message is evidence of the same weight as one
asserting a success.

### 3.2 Live scenarios

Host UI asset deployment uses `SIGUSR1`/`ExecReload` without restarting
the host. Automated offline evidence does not substitute for human review
of the served UI; no paid live run is needed to validate its asset swap.

JSONL terminal classification uses `turn_lifecycle.model_error_kind` and
`http_status` before rendered prose or bracketed physical-attempt summaries.
The typed model-error kind follows a scenario failure into runaway scoring;
a typed Transport failure needs no matching English sentence, and a different
typed kind cannot be relabelled by transport words in its diagnostic.

Some surfaces still have no equivalent structured class. CLI startup lookup
exhaustion exposes only rendered attempt count/cause; compatibility-notice
refusal exposes only account-bearing prose; session-log failed terminals and
child fault strings can lack a model-error kind/status. The capture `.info`
terminal is a Rust Debug value, not structured phase/kind/reset fields. Its
network-failure and capture-cap classifiers must still read that encoding.
JSONL also lacks a distinct accepted-in-band-error bit, so distinguishing that
case from an injected pre-acceptance status still needs its diagnostic marker.
Telemetry has retry cause/transport-phase and credential-refresh failure-class
columns, but turn/model-attempt status rows do not expose the missing terminal
transport facts or startup refusal classes. These are typed-evidence gaps, not
permission to infer a replacement class or change production wire formats in
the live driver. Untyped surfaces retain their existing guarded text oracles.

A startup model lookup that exhausts the bounded GET retry policy (chapter 01
§6.2) scores `provider` only for a real-provider cause: a timeout requires at
least two attempts, and a non-timeout requires three attempts or HTTP 429.
An `operation deadline` is always `harness`, including after multiple charged
dispatches. Early exhaustion otherwise is `harness`, not outage evidence.
When `PLAK_TEST_FAULTS` targets a lookup, attempts no greater than the number of
injected faults score `harness`: the product was required to absorb those
faults. `path=` is a prefix of path and query, including broad `/v1`, `/api`
and `/backend-api/codex` prefixes; an empty path matches all lookup paths on
the selected host. An unbounded matching injection cannot establish a real
provider failure. CLI, JSONL, command and child-output classifiers use the
scenario's fault plan consistently. Malformed catalogs, invalid admission
evidence and absent models do not acquire provider classification.
Transient first-attempt proof scenarios must still bind and complete a real
turn; a retry message alone is not success evidence.

A live scenario drives the real `plakd` binary against a real provider account
and checks its result independently of a clean protocol terminal. It is opt-in,
may spend provider usage, and never runs in any gate tier. Three drivers exist
in `tests/live/` (whose README lists every scenario and its oracle):

- `just live smoke <route>` runs eleven short scenarios in a few minutes across
  `plakd run` and `plakd agent`: a coding task with
  a planted bug, resume, a manual compaction whose earlier fact must survive,
  cancel mid-tool, four transport faults injected with `PLAK_TEST_FAULTS`
  (section 2.8), and a two-child subagent wave with adoption.
  The compaction read's byte-measured observation may hit a small route's
  admitted input ceiling. Only when the driver's catalog window is unknown
  or at most 65,536, a first-turn `ContextLimit` followed by a logged
  `capacity` start and committed compaction is an accepted remedy, not a
  harness failure. The capacity start must follow the first turn's failed
  `turn.lifecycle` terminal. This pass reports `capacity recovery compaction,
  context carried`, distinctly from manual compaction. A ContextLimit on a
  known larger window remains `harness` even if capacity compaction commits.
  In the accepted case smoke uses that compact instead of requesting
  another manual compact; the second turn and fact question must still
  complete, and `ZEBRA-42` must survive without appearing in any repository
  file before the question. A ContextLimit without that committed capacity
  recovery remains `harness`.
- `just live soak <route> [--hours N]` keeps one session doing real tasks on a
  pinned open-source checkout, reopening it in a new process per task, with
  faults, SIGSTOP/SIGCONT sleeps, manual compactions and one mid-run rebuild to
  a byte-distinct `plakd`, and reports throughput, tokens, peak context,
  compactions, errors and dead ends. It is headless. `just live soak <route>
  --long-session --only <names>` does not run that hour loop; it selects the
  named long-session scenarios below. `--all` in place of `--only` selects
  exactly those four. The two flags are mutually exclusive, and a bare
  `long_session.py` launch is refused.
- Opt-in long-session scenarios in `tests/live/long_session.py` are outside
  the default eleven: in-turn compaction, an overflow remedy and continuation,
  SIGKILL mid-tool then reopen, and a child that compacts and still reports.
  The pre-release bar runs all four on every `full` row, via `--all` (§3.4).
  The product caps the text one model round observes (chapter 01,
  observation policy), so one parallel batch cannot fill a large window. The
  overflow turn reads in rounds instead: each round is one parallel batch of
  reads under that cap, and the rounds together pass the route's compaction
  trigger, so the read that passes the trigger lands on an almost full
  window. Each read also
  stays within its own tool's per-call cap (pi `read` returns at most 50 KiB,
  one Codex `exec_command` result at most 40,000 bytes), so a batch on such a
  pack takes more, shorter reads, and `bulk.txt` holds every planned line. On
  the pi pack the plan is phrased as sequential reads, one call per response,
  each under both caps, because its model did not reliably issue one batch per
  round; a pi round is then a single read, so the sequential reads still pass
  the trigger and the read that passes it lands on an almost full window. On
  the Codex pack the growth and overflow `bulk.txt` is the planned reads as
  pages, each a header line with a random check word and the next page (or
  that it is the last) followed by that read's lines, read one page per Code
  Mode exec through a pager script, with no loop in an exec and no
  `max_output_tokens`; the model writes each page's check word to
  `pages/<k>.txt`. The Codex growth turn must write every page's word; the
  overflow turn reports the count, which a `model` miss names. Sizing converts bulk bytes to tokens at the pack's measured bytes per
  token, and a pi growth `bulk.txt` holds only the reads that pass the trigger
  plus a few more. The overflow
  remedy is whichever the session log shows: an in-turn
  `scheduled` compact, a `context.placeholders` shrink, or a failed
  `ContextLimit` turn followed by a `capacity` compact. `just live smoke <route>
  --only <names>` selects them too. Oracles are session-log and file facts,
  not the model's own claims. A missing compact is `harness` only when
  `model.operation_usage` shows prepared input at the catalog window minus
  one read page and no remedy committed; otherwise it is `model`. The child
  scenario does not use that split and does not read a child event log
  (there is none; JSONL v1 does not project child transcripts). After the
  root agent quits, child `cN` is taken from the root's JSONL `tree` frames
  or spawn/wait receipts, or from `plakd session show <root>`. `plakd session
  log <root>.cN --json` is the child's committed canonical history. A
  committed compaction segment projects as `kind` `compaction` with display
  text `compacted N earlier segments (<pass>)` (or `compacted earlier
  segments` when the checkpoint's provenance does not name it). The summary
  ciphertext is not that text, and an empty assistant item is not the
  segment. `acknowledged.txt` and
  `recalled.txt` in the adopted checkout must match the fact file. The fact
  is a neutral code word in `codeword.txt`, except on the Codex pack, which
  read a code word as a secret it must not reproduce: there it is the
  project's release label in `release_label.txt`, which a later step needs.
  On every pack the oracle is that the copies match the fact file and that
  no fact file is read again after the compaction. That
  projection carries no `model.operation_usage`, so a history without the
  segment is `model`. `pending` is a failure of `session show` or `session
  log` itself, not a missing segment. Chapter 07
  has no key that lowers a context window or compaction trigger; the scenarios
  pin `[compaction] enabled` and `[turn] max_model_rounds` on a private home
  and size `bulk.txt` to the route's window. The pinned round cap is the
  scenario's planned rounds (each planned read, two per check-word page, or a
  child's pages at two each) plus 32, at least 128 and at most chapter 07's
  1024, and the report records it per scenario as `round_cap`. The oracle
  runs first. A `model` miss by a turn that ended on the operation-budget
  wrap-up (the session log's `turn.operation_budget_exhausted`, or the
  wrap-up note in a child's captured requests) is `harness`, driver sizing,
  only when the plan with its headroom does not fit under the pinned cap;
  otherwise the model spent the cap and the miss stays `model`. An
  OpenRouter route's window is
  the session's admitted input ceiling, which the operator supplies as
  `PLAK_LIVE_INPUT_CEILING` (§3.4); without a valid value the driver starts no
  process and reports a `pre_attempt_error`. The long-session report records
  the window used and its source as `input_ceiling`.
- `just live jobs <route> [--only NAME,...]` runs seven bounded background-job
  scenarios: promotion at `[jobs] foreground_seconds`, a steer during a
  foreground command and during an await, a completion notice while busy,
  `plakd run` waiting for a promoted job, crash reopen as outcome-unknown, a
  child submit refused with `live_work` while its job is live and then settled
  with that job terminal, plus a parent close that stops another live child
  job with `attempt ended`. It injects no transport faults. The
  pre-release bar runs all seven on every `full` row (section 3.4). Any other jobs
  run is opt-in. The dry-run estimator prices that full-row cell (§3.4).

- Opt-in feature scenarios in `tests/live/features.py`, selected with `just
  live smoke <route> --only <names>`, cover image input, `fetch_url`, MCP over
  stdio and streamable HTTP (the reference `server-everything` and the
  official read-only GitHub server), hosted `web_search`, `web_fetch` and
  `x_search`, and an image-generation round trip into a vision route. Each
  run's private home lists exactly the optional tools it needs. Oracles are
  `plakd session log --json` tool calls, the raw capture (an image block in
  the request), the MCP server's own records, and values the model cannot
  guess (a SHA-256 in a file at a public commit, random operands, a token only
  the server holds); a retrieval fact must appear on a cited page the driver
  fetches itself. A route row that plakd refuses at bind or admission scores
  `refused`, which is the product's statement rather than a failure.

All three build `plakd` with `test-support`; only the smoke and headless soak
inject faults. Each result is
`pass`, `harness` (plakd's failure, including a dead end: a failed turn after
which a plain follow-up also fails), `model` (a completed turn whose work
misses the oracle), `provider` (the real provider refusing without an injected
fault) or `pending` (a failure a named in-flight change is expected to fix).
An injected fault before acceptance must be absorbed and the turn completed.
A stream cut after acceptance but before any tool call on a cost-only turn
request replays once with its partial output discarded (chapter 01, replay of
cost-only requests) and the turn completes with exactly one effect; a break
that could repeat a server effect fails the turn without replay. Either way
the session must take the next prompt. `tests/live/STATUS.md` records the latest
runs.

A published pass names the qualification tuple in §3.4. `tests/live/STATUS.md`
may group rows only when those facts share one recorded revision; evidence
without that revision, or from a dirty tree, remains local diagnostic evidence
rather than publication qualification. A pass does not admit a different tuple.
A later revision is admitted only under the re-confirmation rule in §3.4, and
each cell that rule requires is a new run. A catalog listing, a successful
decode, or a model that is simply reachable authorizes nothing. Transient provider failures (HTTP 429, post-
acceptance rate limits, ambiguous 5xx responses) are recorded as provider
outcomes, never retried silently and never relabeled as product failures.

The same `provider` class covers every surface the drivers score: a JSONL
turn failure, a `plakd run` exit, a manual compact operation
refusal, and a child failure. A typed provider refusal or outage is
`provider`: `RateLimited` or HTTP 429, `Unavailable` (including an OpenAI
`server_is_overloaded` terminal), `QuotaExceeded`, a refusal
rejected before acceptance, and a model refusal terminal (the provider's own
`stop_reason` `refusal`; the driver keeps its typed fields, never its prose). A
provider error delivered inside an accepted Chat stream is the provider's own
outage, not a protocol violation: an in-band error the product types
`RateLimited`, `Unavailable` or `Timeout`, or whose diagnostic names 429 or a
5xx with its class (`… returned an in-band error after accepting the request:
HTTP 504 (timeout) …`), is `provider` on every surface, like that status before
acceptance; any other in-band error is `harness` unless it is typed provider
elsewhere (`Authentication`, `QuotaExceeded`). Provider text is matched only
outside the bracketed attempt summary (`[N requests; last: …]`), whose label
names a physical request, never the terminal's cause, and a typed HTTP status
that is not a provider status outranks provider prose. A kernel stream-validation failure is `provider`
only when the raw exchange capture shows the provider's response violated
the documented protocol. The evidence rule is: some `exchange-*.body` in
that turn's capture parses as JSON, or as an SSE `data:` JSON object, and
that object carries `usage` together with a `choices` entry whose
`finish_reason` is missing or null (the chat rule that usage is one
post-finish terminal-metadata chunk). A missing capture, a truncated
capture, or a body that does not show that violation stays `harness`. A
complete, integrity-checked capture whose finished Chat tool calls or Messages
tool-use blocks contain malformed JSON arguments is `model`, not `harness`:
the model emitted invalid tool input and the kernel correctly failed closed.
This evidence is restricted to the failing process's capture segment and its
highest-numbered exchange, ordered by the integer exchange number rather than
the filename. That exchange must be `Complete`, `ProtocolComplete` or
`ClosedEarly`: a kernel that fails closed on finished arguments stops reading
there and closes before end of body, so the finish evidence in the body below,
not end of body, shows the arguments were complete. A later unfinished
exchange prevents earlier exchanges from serving as model evidence. An earlier scenario, process, or exchange cannot relabel a later
harness failure; multiple process
segments without an identified failing process are insufficient evidence.
The full kernel diagnostic, including the protocol's `stream validation
failed` prefix, must match its own protocol: Chat malformed/ambiguous arguments
require Chat tool-call evidence; invalid Messages tool input requires Messages
tool-use evidence. Cross-protocol captures and bare diagnostic suffixes are
not model evidence. Today only the OpenRouter Chat and Messages provider paths
emit these prefixed diagnostics. Other providers' argument failures remain
`harness`; malformed arguments alone cannot establish this model exception.
Chat argument fragments are joined per choice and tool-call index; duplicate
object keys are also invalid. Messages fragments are joined per tool block
and require its close plus the message terminal. Messages input permits
duplicate keys (the last value wins), matching its kernel; both protocols
reject non-JSON constants and require an object. This exception requires one
of those three transport terminals (an `Abandoned` or `Failed` exchange is not
evidence) and paired request/body/metadata with matching recorded byte length; missing, truncated,
unfinished, or valid-argument captures stay `harness`. A model-selected job
stop (`stop_reason` `model`) is `model`, not `harness`.
Output-limit outcomes score alike on every surface. A tool-free `Length`
answer with visible text is a completed turn (chapter 01, length
classification): `plakd run` prints it and exits 5 with
`turn_failed:output-limit`, and the drivers treat that exit as the completed,
truncated turn JSONL, so the scenario's oracle judges the
printed answer. A failed turn of typed kind `OutputLimit` (a second
consecutive empty `Length`, or one that may not retry) is `model`: the model
spent its output bound without an answer. JSONL lifecycle frames carry the
kind; `plakd run` stderr and child
failure text carry only its rendered diagnostic (`response reached its output
bound before completion`, or the Responses provider's `stopped at the output
bound before` message), which identifies it there; on `plakd run` only the
final non-empty stderr line, the turn's own failure, is read for it. Like any first `model` it
gets the single MODEL rerun (§3.4).
A `plakd run` launched by a driver has stdin closed, so a subscription
compatibility notice that still needs acceptance fails at once (exit 3); that
is `harness`, and the reason names the remedy, `plakd models <account>`.
An injected fault the scenario required the product to absorb stays
`harness` when the turn fails with that same fault; a different provider
refusal during that scenario is still `provider`. An injected fault that was
never observed is `harness`, with one exception: an `after-events=<n>` stream
fault that never fired because the provider ended the targeted exchange first.
The targeted exchange is the lowest-numbered request in the faulted process's
segment that carries a conversation (`messages` or `input`; model lookups are
bodiless GETs). It qualifies when it ended (`Complete`, `ProtocolComplete`,
`ClosedEarly`, or a network-shaped transport failure as below) before its
Nth event, counted as the fault counts them (§2.8: comment-only blocks such as
keep-alives are not events), in a provider failure, and the turn failed as a
provider error (an in-band provider kind `RateLimited`, `Unavailable` or
`Timeout`, or a provider HTTP status), which a transport kind is not, or
completed after the product retried it: the turn records a transport or
provider retry and the segment holds a later request with the same
conversation, as for a provider HTTP error that consumed the faulted request. The
injected cut lands exactly at the Nth event, so it never qualifies. That
outcome is `provider`.

A provider failure in a capture is one of: an HTTP status of 408, 429 or a
5xx; a well-formed in-band error as the body's last counted event other than
Chat's `data: [DONE]` (Chat: an `error` object with an integer `code` of 408,
429 or a 5xx; Messages: `{"type":"error","error":{"type":T}}` with T
`rate_limit_error`, `api_error` or `overloaded_error`; Responses:
`response.failed` whose `response.error.code`, or an `error` event whose
`code`, is `rate_limit_exceeded`, `server_error` or `server_is_overloaded`); or
a network-shaped transport failure in `ResponseHead` or `ResponseBody`: `Io`,
`Tls`, `DeadlineExceeded`, or `Protocol` with an HTTP/2 reset reason.
`Internal`, `LimitExceeded`, `InvalidResponse`, a cancellation and any other
`Protocol` failure are not provider failures: they earn no credited wait and
no resend allowance.

A driver's `plakd run`, an awaited `plakd agent` turn (the jobs driver's
included), that outlives its
scenario budget is `harness` (a product hang) unless the timed-out process's capture segment proves the
provider used that budget. The driver accounts the window from the kill back
over the budget using file modification times. Each exchange spans dispatch
(its `.request`) to its terminal record (its `.info`); a pending exchange,
whose highest-numbered record still carries the pre-dispatch `status=None`,
`terminal=pending`, `incomplete=true`, runs to the kill. Spans of in-process
children that share the segment are united. Provider time is the pending
exchange, exchanges that ended in a provider failure, and the part of each
successful exchange its own work does not explain: its duration less a fixed
5 s first-token and prefill allowance and less its reported output tokens
(reasoning included: Chat `usage.completion_tokens`, Messages
`usage.output_tokens`, Responses `response.usage.output_tokens`) at 15 tokens
per second, counted from its dispatch, and none of it when the response
reports no usage. The rest of the exchange time is token-explained, so a loop
of healthy rounds is mostly explained by its own work and does not by itself
reach the provider share below; only rounds slower than that allowance and
rate contribute. After a provider failure, the gap
until the next request of the same work (a lookup's next lookup, or a request
whose conversation extends the failed one) is a credited wait of at most 30 s
plus 5 s of slack: the capture records no response headers, so Retry-After
advice (chapter 01 §13.1 honors up to 60 s) is not visible, and chapter 02's
transient continuation waits 30 s by default. The rest of the window is local.

The timeout is `provider` (`provider did not respond within the scenario
budget`, with the provider, token-explained, credited and local seconds and
the state the kill found) only when all of these hold: the scenario injected no
faults; the segment passes capture integrity apart from that one pending
exchange; every exchange that ended inside the window has a protocol terminal
(`Complete`, `ProtocolComplete`, or `ClosedEarly` after a terminal event) or a
provider failure; the kill lands inside a credited wait, or on a pending
exchange that is not stale; local time is at most 15 s; provider time and
credited waits are at least half the window; at most six model lookups ran in
the window; and there is no request loop. A pending exchange is stale, and the
product's to end, when its `.body` carries a protocol terminal event (Chat
`data: [DONE]`, Responses `response.completed`, `response.done` or
`response.failed`, Messages `message_stop`); when a lookup has waited more than
its 60 s cap plus 5 s; when a non-empty body received no byte (keep-alive
comments and pings included; by its `.body` mtime) in the 60 s before the
kill; or when an empty
body has waited, from its `.request` mtime, more than the route's dispatch
deadline plus 5 s (120 s for the OpenAI Platform, xAI API and Anthropic API
routes and when the driver does not name the route, 300 s otherwise). A
request loop is an in-window request whose conversation (its `messages` or
`input` items) equals or is a prefix of an earlier request's, except an
identical resend after that earlier exchange's provider failure, dispatched at
least 0.4 s after it ended (the shortest backoff without advice), at most
three times for one request (four physical requests per logical attempt;
a transient continuation grows the conversation and is not a resend). A shorter conversation that is not such
a prefix, such as a compaction, is not a loop, and interleaved agents keep
distinct conversations. Anything else keeps the timeout `harness`. Capture
completeness then excuses exactly the one pending exchange in that segment,
and only on a scenario scored `provider` this way: the scenario evidence lists
the segment under `provider_pending`, and every other exchange in it and every
other segment must still be complete, so the cell is not relabeled `harness`
solely because the killed process left that exchange pending. A report whose
capture is incomplete names the scenarios, segments and exchanges at fault.

The `test-support` capture is required and stops at its 8 MiB response cap; a
response that outgrows it aborts its exchange in the body
(`Failed(ResponseBody, Internal)`). An exchange whose capture stopped exactly
there (HTTP 200, `recorded_bytes` 8,388,608 and equal to the body,
`incomplete=true`, failed `Internal` in `ResponseBody`) while every complete
SSE event of its body is JSON stream preamble or model output for one output
item (a Responses item's deltas, a Messages content block's, or one Chat
choice's text or one tool call's arguments) is a runaway model output, not an
evidence gap: capture integrity accepts it on every driver. In smoke,
long-session and jobs a `harness` or `model` scenario failure scores `model`
(`model output ran past the 8 MiB capture bound (runaway output in <item
kind>)`) only when the runaway explains it: the failure is a turn that ended
on the aborted stream (reported as a transport failure of the response
stream), and the runaway exchange is in the scenario's last process segment
and is the last exchange there that received a response byte. Any other
failure in the same scenario keeps its class.
The headless soak keeps its own task outcome. Every other incomplete
capture stays `harness`.

A live run uses an isolated, disposable `PLAK_HOME` under the ignored
`target/live/`; the ordinary home is never imported. OAuth subscription credentials are the
deliberate exception: they are XDG state at
`${XDG_STATE_HOME:-$HOME/.local/state}/plak/subscriptions/<provider>/credential.json`,
outside `PLAK_HOME`, with no per-run copy, so a refresh during a live run writes
the real credential store. API-key routes instead copy the key installed by
`plakd key set` from the ordinary home into the disposable home and remove the
corresponding environment variable from child processes, preventing a stale
variable from shadowing the file without printing credential bytes.

### 3.3 The support rule

Publication and Rust nameability confer accessibility, not support. Neither does
a passing test on an adjacent surface. Every claimed API belongs to an explicit
supported inventory, and publication or internal nameability is kept distinct
from qualification and compatibility promises. The `plakous_agent::internal`
tier is deliberately nameable so official sibling crates can compose against it;
that accessibility is not a support promise, and `#[doc(hidden)]` is not a
boundary.

### 3.4 The pre-release bar

The pre-release bar is the evidence a release candidate must carry before
that revision is qualified. It runs once per release candidate, not per
install. It has three parts, all bound to the same source revision:

1. `just full` on both supported platforms, `just musl native` and
   `just musl container`, the full strict crash-point sweep (§2.3) with every
   REFUSED signature reviewed against its allowlist, and the publish dry run;
2. the live matrix below;
3. maintainer review of the candidate host through an independently maintained
   client, separate from public automated gate prerequisites.

The live matrix is opt-in and paid. It is not a gate tier, and a gate pass
does not satisfy it. Library and facade evidence stays the offline fixtures
(§3.1) and the public compile contracts (§1.1). The bar spends live time on
what daily use does not exercise (routes the operator does not run, crash and
kill timings, overflow, jobs) rather than repeating long soaks that daily use
covers.

A qualification run binds one tuple. The `test-support` `plakd` named in the
report was built from a clean checkout of one source revision, including the
live drivers. The report names that revision, the host, and the target triple
(`aarch64-apple-darwin` or `x86_64-unknown-linux-musl`). It names the SHA-256
of that `plakd` at the start of the run and at the end, and the SHA-256 of the
`plakous-js-host` and `plakous-py-host` workers from the same build. It names the
account, whether the account is a subscription login or an API key, and the
exact model, effort, protocol and composition. The composition for this bar
is `plakous-default`; the report names it as `composition_requested` and names
`composition_bound`, every composition the run's `plakd agent` hellos reported,
because a pack may bind the request to its own composition (Codex binds
`plakous-default` as `native-ptc`). It names every scenario in the cell. A
dirty tree, a digest that changes mid-run, or any other tuple is diagnostic
only. The headless soak's intentional rebuild to a byte-distinct `plakd` is part
of that scenario: both digests are recorded, and any other digest change is not
a qualification.

Route names are the keys of `ROUTES` in `tests/live/live.py`. Account, model,
effort and protocol are that route's fields. The routes are the owner's
catalog and nothing else:

- Codex: `gpt-6-luna` at low (`codex`) and medium (`codex-luna-medium`),
  `gpt-6.1-sol` at low (`codex-sol`), `gpt-6-astra` at low (`codex-astra`).
  GPT-6.2 Sol at low and medium (`codex-sol62-low`, `codex-sol62-medium`)
  and GPT-6.2 Astra at low and medium (`codex-astra62-low`,
  `codex-astra62-medium`) are light smoke rows only, not long-session or soak
  qualification. Sol is rung 2; Astra is rung 3 (sparing).
- SuperGrok: `grok-4.6` at low (`grok-46`), `grok-4.7` at low (`grok`).
- Claude: `claude-haiku-5-5`, `claude-sonnet-5-5` and `claude-opus-5-5` at low
  and medium (`claude-haiku55-low`, `claude-haiku55-medium`,
  `claude-sonnet55-low`, `claude-sonnet55-medium`, `claude-opus-low`,
  `claude-opus-medium`), `claude-fable-5-2` at low (`claude-fable`, adaptive
  thinking at the selected effort).
- OpenRouter free, best effort: `nvidia/nemotron-3-ultra-550b-a55b:free`
  (`or-nemotron-free`), `nvidia/nemotron-3.5-super-vl:free`
  (`or-nemotron-vl-free`) and `recursive/flint-v1.0:free` (`or-flint-free`).
- OpenRouter paid, not best-effort: `deepseek/deepseek-v4-flash-0731` on Chat
  (`deepseek`, no protocol) and on Messages (`deepseek-messages`, protocol
  `messages`), `z-ai/glm-5.3-flash` (`or-glm`).
- API keys: OpenAI `gpt-6-luna` at low (`api-openai`), Anthropic
  `claude-haiku-5-5` at low (`api-anthropic`), xAI API `grok-4.6` at low
  (`api-xai`). OpenAI also has `gpt-6.2-sol` at low (`api-openai-sol62-low`,
  rung 2) and `gpt-6.2-astra` at low (`api-openai-astra62-low`, rung 3, sparing):
  light smokes only, with no long-session or soak requirement.

Depth follows the account. The Codex, SuperGrok and Claude subscriptions and
OpenRouter are tested deep: they carry every `full` row. API keys are very
light: each has a `light` row on its cheapest first-rung tuple; OpenAI also
has the two explicit GPT-6.2 light smoke rows.

- `full`: `codex-sol`, `grok`, `claude-sonnet55-low`, `deepseek`,
  `deepseek-messages` (both DeepSeek rows' long-session lanes are
  informational, below).
- `light`: `codex`, `codex-luna-medium`, `codex-astra`, `grok-46`,
  `claude-sonnet55-medium`, `claude-haiku55-low`, `claude-haiku55-medium`,
  `claude-opus-low`, `claude-opus-medium`, `claude-fable`, `or-glm`,
  `api-openai`, `api-anthropic`, `api-xai`, `codex-sol62-low`,
  `codex-sol62-medium`, `codex-astra62-low`, `codex-astra62-medium`,
  `api-openai-sol62-low`, `api-openai-astra62-low`.
- `informational`: the three free OpenRouter rows; also the `deepseek` and
  `deepseek-messages` long-session lanes.

Each route has a `rung` on the testing ladder. A qualification cycle runs the
rungs in order, cheapest first, and a `harness` outcome on a lower rung is
fixed before a higher rung spends.

1. `FIRST`: the free OpenRouter rows, `codex`, `claude-haiku55-low`,
   `grok-46`, and the three API rows.
2. `THEN`: `codex-sol`, `claude-sonnet55-low`, `grok`, `deepseek`,
   `deepseek-messages`, `or-glm`, and the medium-effort rows
   `codex-luna-medium`, `claude-haiku55-medium` and `claude-sonnet55-medium`.
3. `CONFIRM`: `claude-opus-low`, `claude-opus-medium`, `codex-astra` and
   `claude-fable`. These rows are sparing: they run only for a targeted
   confirmation, the maintainer naming the row and the change it confirms,
   and are left out of default sweeps and the default estimate. A confirmation
   runs at `light` depth and scores like a `light` cell. The bar does not wait
   for a sparing row that no confirmation named.

`codex-sol` (`gpt-6.1-sol` at low) is the full Codex row. `gpt-6-luna` at low
and at medium ended long growth turns early ("I can't finish paging ...
within this turn", after 37 to 41 of 52 pages) and skipped a page after the
child's compaction in every long-session cell of a qualification cycle, with
plakd's requests verified against this chapter; `gpt-6.1-sol` paged all 69
growth pages through compaction under the derived round cap and passed
in-turn compaction, the child (10 of 10 pages) and kill-reopen. Its overflow
attempt in that diagnostic was a model runaway (one tool call's input
repeated until the 8 MiB capture bound), which the runaway rule in §3.2
scores `model`. `codex`, `codex-luna-medium` and `codex-astra` stay `light`.

A route may mark single lanes informational (`informational_lanes` in
`ROUTES`). Such a lane still runs at its row's depth and is scored like an
informational row; the row's other lanes keep its class. `deepseek` and
`deepseek-messages` mark long-session, so no OpenRouter row has a blocking
long-session lane. The marking carries from the DeepSeek flash model these
rows ran before (`deepseek/deepseek-v4.1-flash-1017`) until
`deepseek/deepseek-v4-flash-0731` has its own long-session evidence. The
earlier model's own compaction summary lost the earlier turn's fact: replaying plakd's
captured summary request through OpenRouter, on Chat and on Messages, the
summary dropped that fact once one or two large tool results followed it, at
about 34k to 70k tokens as well as at about 826k (none of 18 kept it from 100k
to 826k; three of three kept it with no bulk; with tool definitions present it
kept the fact at about 38k and lost it at about 71k). plakd's summary input was
complete and byte-equal to the request before the fold, and Grok and Claude
pass the same lane. Deep into long tool loops, that model over OpenRouter
Messages also sent `read` calls with only a path, even given an argument
example, while the same model and upstream on Chat paged correctly with the
same tool schema. Both are model or provider translation limits, not product
behavior. Smoke, jobs and the headless soak stay blocking on both rows and
cover the Chat and Messages dialects.

The three API rows are `light` and are not best-effort.

Apple Silicon runs every row at the depth of its class, sparing rows only when
a confirmation names them. Linux x86_64 musl runs only the eleven-scenario
smoke, and only on the `full` rows and the three API rows `api-openai`,
`api-anthropic` and `api-xai`.

On Apple Silicon, §3.2 names the oracles:

- `full` runs the eleven smoke scenarios, all four long-session scenarios,
  and all seven jobs scenarios, with `crash_jobs` run three times. One `full`
  row, the one the maintainer uses least, also runs the headless soak at the
  driver's default of one hour.
- `light` runs the eleven smoke scenarios.
- `informational` runs the eleven smoke scenarios. The result is recorded. A
  `model` or `provider` outcome does not block the bar. A `harness` outcome
  does. An informational lane of a `full` row is scored the same way.

On Apple Silicon the full cell of a `full` row, with `<route>` that row's
name, is these commands; the soak line belongs only to the least-used row.
On `deepseek` and `deepseek-messages` the long-session line is informational:
it runs and is recorded, a `model` or `provider` outcome does not block, and a
`harness` outcome does. The cell driver records the scored class as `lane_class` in its
binding.

```text
python3 tests/live/qualification.py smoke <route>
python3 tests/live/qualification.py long-session <route> --all
python3 tests/live/qualification.py jobs <route>
python3 tests/live/qualification.py jobs <route> --only crash_jobs  # twice more: three crash_jobs runs
python3 tests/live/qualification.py soak <route> --hours 1          # least-used full row only
```

On the OpenRouter `full` rows (`deepseek`, `deepseek-messages`) the
long-session line runs with `PLAK_LIVE_INPUT_CEILING` set to the session's
admitted input ceiling: run a short session on the same route and protocol
and take `admitted_input_ceiling=N` from `plakd usage --session <id>`, which
prints `context_status=... prepared_input=... total_context_window=...
admitted_input_ceiling=N`. Chat and Messages are read separately. The cell
driver refuses an OpenRouter long-session cell without a valid value before
any paid attempt, recording the remedy as its one attempt's reason, and the
cell binding and the long-session report record the value and its source.

The qualification commands select the same oracles as the raw single-attempt
diagnostics: `just live smoke <route>` is the eleven scenarios; `just live soak <route>
--long-session --all` is exactly the four long-session scenarios. `--all` and
`--only` are mutually exclusive, a bare long-session launch is refused, and
`--only` does not fill the cell. `just live jobs <route>` is all seven scenarios.
The headless soak is the driver default of one hour; a shorter soak does not
fill the cell and a longer one does. Linux x86_64 musl's cell is
`python3 tests/live/qualification.py smoke <route>` on each `full` row and on
the three API rows.

A refused reopen in `crash_jobs` or `kill_reopen` is a `harness` outcome. The
drivers record the refused head's decoded shape and bucket it as covered,
known-uncovered or unknown; the bucket is diagnostic and does not change the
class.

**Maintainer sign-off.** A maintainer reviews the candidate host through an
independently maintained client. No private client repository is a prerequisite
for public host gates, builds or source checkouts.
`just live daily-driver --since <date>` summarises that use from the product log and
telemetry, read-only, per installed build (active days, session bindings,
warnings and errors by target, retries, reloads, carries, recovered or
abandoned turns). It is a diagnostic, not a cell, and cannot stand in for a
missing cell.

Every independent oracle has to be `pass` on a blocking cell. Classes are
§3.2's. The raw drivers do not retry inference inside a run (§5).
`tests/live/qualification.py <lane> <route> [lane options]` drives a cell's
attempts for `smoke`, `long-session`, `jobs` or `soak`. Each
attempt runs the whole selected lane in a fresh artifact directory and home,
with a new session and the same route, build and prompt. Reports and workspaces
stay under `target/live/qualification-<lane>-<route>-XXXXXXXX/attempt-N/`.
All socket-bearing live scratch homes, including scenario-specific homes,
use unique owned mode-0700 directories under the resolved physical system
`/tmp`, not below artifacts or a potentially deep inherited `TMPDIR`.
`homes.json` records their locations as soon as allocated; completed reports
include the same `homes` map. Homes are retained on success and failure like
artifacts, and are removed with those artifacts by deleting the listed
directories (the zero-inference MCP OAuth lane records `home_bases` and removes
them in its existing teardown). Before launch, the harness refuses symlinked
paths and checks the current state-key host socket, home-local runner socket and retention
socket in filesystem bytes against both SUN_LEN limits (104 on macOS, 108 on
Linux), including NUL and eight bytes of margin. A path that does not fit
is `harness`, with a short-temp-root/state-key remedy and no paid attempt.
Randomized long-session and jobs fixture inputs use the same seed on both attempts. The
headless soak verifies its retained starting binary after copying and before
restoring. It records `binary_sha256_rebuilt` immediately after its own rebuild
and requires the end digest to equal that digest. Before a rerun it restores
only when the current binary still equals that intentional rebuild, never over
an operator's newer build. Each attempt records the current, intentional and
restored digests and `starting_build_restored`. A pre-attempt mismatch records
`binary changed before attempt; no paid attempt`; a missing or invalid
OpenRouter `PLAK_LIVE_INPUT_CEILING` on a long-session cell is refused the same
way, scored `harness` with its remedy and no paid attempt. Missing or incomplete captures
(other than the one pending exchange a provider timeout leaves, §3.2),
dead ends and unexpected build changes are `harness`, not a reason to spend
another attempt.

Scoring normalizes the first word of a soak outcome, so `model after
stream:reset` remains `model`. HARNESS evidence failures precede PENDING,
PROVIDER and MODEL. A MODEL reason names the task, its evidence status and
captured-exchange count, and its capture segment(s); a MODEL record without
that reference is HARNESS. Forwarded lane options are exact (no abbreviations)
and validated even in dry-run; a lane parser exit is retained as a HARNESS
cell report rather than losing the attempt.

- A `harness` outcome blocks on every row, including informational and
  best-effort. It is not relabeled, waived or rerun.
- A cell whose first attempt is `model` may rerun exactly once as a fresh
  attempt of the same tuple and prompt. The rerun is final: `pass` qualifies;
  anything else counts under its class, with no further rerun. A final
  `model` blocks on `full` and `light`; on `informational`, including an
  informational lane, it is recorded and does not block. A `provider` on this MODEL rerun is not the second PROVIDER
  outcome and receives neither another attempt nor the best-effort waiver.
- A `provider` outcome is run again once, as its own scenario run of the same
  tuple, on every class. The rerun is classified on its own. The cell passes
  if the rerun is `pass`. A `harness` outcome on the rerun blocks. A `model`
  outcome on the rerun blocks on `full` and `light`. A second `provider`
  outcome blocks, except on a best-effort row, where it is recorded and does
  not block. A `model` on this PROVIDER rerun does not receive a MODEL rerun:
  only a first-attempt `model` is eligible. Rerun allowances do not chain.
- A `pending` outcome does not pass.

The qualification report and printed summary keep both attempts in order:
the first classification with its capture-proven reason and evidence, then
the final rerun classification and evidence. `PASS after MODEL rerun` is
distinct from a clean `PASS`; the first failure is never overwritten.
The dry-run command prints the selected lane and its maximum of two attempts.
`tests/live/estimate.py` reserves one possible extra attempt per cell,
explicitly priced beside the clean-attempt subtotal and before contingency.
The PROVIDER and MODEL rules share that reserve, not successive attempts.

Best-effort rows are the informational rows, the free OpenRouter models; an
informational lane is best-effort too. The paid OpenRouter rows `deepseek`
and `deepseek-messages` outside their long-session lanes and `or-glm`, every
subscription row, and `api-openai`, `api-anthropic` and `api-xai` are not
best-effort.

A later revision is re-confirmed by a new eleven-scenario smoke on each
`full` row, on Apple Silicon and on Linux x86_64 musl. That smoke does not
by itself repeat light rows, informational rows, long-session, jobs or the
soak, and it does not replace the offline gate.

A row that was never qualified, or whose tuple changed, has no earlier pass
to carry: it runs at its class depth before the bar is met (a sparing row
only when a confirmation names it).

In addition, every row whose own rendered-prompt digests (§2.5) or harness
footprints (chapter 02 §6.7) differ from the passing revision reruns at its
class depth. A `full` row reruns the whole cell, and that cell's smoke is its
re-confirmation smoke, not a second one. A `light` or `informational`
row reruns the eleven-scenario smoke, and the Linux smoke when that row has
one. A row whose own digests and footprints are identical carries the earlier pass.

A row's digests are the pack and family render pins for that route: the §2.5
prompt, lifecycle and wire pins for the family stem that route uses
(`codex`, `grok`, `claude`, `openrouter-chat`, `openrouter-messages`,
`openai-api`, `anthropic-api`, `xai-api`) and the packs `plakous-default`
renders there. Its footprints are that route's rows in the chapter 02 §6.7
matrix. Rows that share those pins and footprints move together. A pin or
footprint outside that set does not move the row.

Before a paid matrix or that re-confirmation, run
`just live estimate --rate-card PATH [--openrouter-ceiling N]
[--openrouter-window N] [--confirm ROUTES]`. It prices the cells from a local
rate card and does not fetch a price, and prints them in ladder order. The
OpenRouter ceiling is an operator input. This chapter states no amount. The
estimate warns when the OpenRouter cells, including the contingency, exceed
that ceiling, and it still prints every cell. It prices a Mac smoke for every
route except the sparing rows, which it adds only when `--confirm` names
them. For each `full` row it also prices the four long-session scenarios, the
seven jobs scenarios plus two further `crash_jobs` runs, and a Linux smoke,
and prints an informational lane's cell with class `informational`; it prices
one Mac headless soak for the row named by `--soak-row`. The tree pins no
OpenRouter context window, so an OpenRouter long-session cell is scaled by
`--openrouter-window`, the model's context length from the OpenRouter
listing, and the estimate is refused without it. For `api-openai`,
`api-anthropic` and `api-xai` it also prices a Linux smoke. An API-key row,
including OpenRouter and those three, is priced when the card names its model
id. A subscription row is quota and stays unpriced even when the card names
its model. An API model the card omits is unpriced and left out of the total.
The printed total is not evidence and is not the ceiling.

This bar does not cover:

- the gate tiers in §1
- any route not listed above, and a sparing row no confirmation named
- `wrap_up`, `py_ptc`, and any scenario the class lists do not name
- a composition other than `plakous-default`, or an effort other than the
  route's
- a Linux run of a subscription light row, an OpenRouter light or
  informational row, long-session, jobs, or the soak
- `x86_64-apple-darwin`
- the Harbor bakeoff (`eval/`), which is never a qualification row (§1.2)
- library or facade support
- a catalog listing, a successful decode, a liveness ping, or the dry-run
  estimate
- an earlier pass standing in for a later revision, other than a row that
  the re-confirmation rule above says carries that pass

### 3.5 Private provider wire captures

Automatic byte-pressure retention protects failed, abandoned and incomplete
exchanges individually, not their entire 128-exchange segment. Successful
neighbors in closed segments may be removed; `retention.info` omission ranges
exclude protected exchanges, whose bytes remain charged. Repeated incomplete
capture warnings are summarized per static reason at most once per 60 seconds
with a suppressed count; capture drop flushes remaining suppressed summaries; capture failure does not stop inference.

Raw wire captures (§2.5) are private local observations, not qualification
receipts or committed fixtures. Ordinary maintainer capture lives under
`PLAK_HOME/wire-captures/`; explicit qualification and reference-client
captures live in the ignored `tests/provider-authority/`. The repository
ignores that whole directory: never `git add -f` it, and never quote its
contents in a report, issue or test log. Request and response bodies carry
prompts, reasoning and task content.

For an explicit qualification capture outside the live drivers, create
`tests/provider-authority/` with mode 0700, a private mode-0700 run directory
under it, and a fresh private mode-0700 directory under the run for each
process, and set `PLAK_TEST_EXCHANGE_CAPTURE_DIR` to that process directory's
absolute path for `plakd run` or `plakd agent`. Never reuse a process
directory. The live drivers do this per launch and reopen and report capture
integrity (`tests/live/README.md`). A successful turn with zero `.body` files
is not wire evidence, and instrumented timing is not production performance.
To keep an automatic run past startup retention, copy it privately outside the
managed root (or remove its lock marker only after the process exits).

A structural comparison against a reference client records the client name and
exact version, the model, the account kind (never its identity), the route
and URL path, the request protocol family and the capture conditions. It uses
a disposable task and the client's real interactive route: Claude Code
interactive (its `--debug api --debug-file` output may omit request bodies),
Grok Build, or the official Codex CLI. `codex debug prompt-input` is
zero-inference; an `exec` coding turn spends usage. A local TLS/CONNECT
recorder or `just prompts native-compare` observes structure with synthetic
credentials and mock responses; only a live request proves service acceptance,
and neither is inferred from the other.

Shareable facts are extracted, never copied. Before extraction remove
authorization and cookie headers, credentials, prompt and tool content,
reasoning and signatures, account and session IDs, e-mail addresses, private
paths and host-specific metadata; keep only event types, field names, block
order, safe enums and anonymized lengths. Review the extracted observation for
leaked values before moving only that sanitized observation into a test
fixture, and prefer synthetic examples. A fixture derived this way names the
request path and client version each structural assertion witnessed and which
provider behavior remains untested.

## 4. Reference product homes

`docs/reference-config/` holds the annotated reference plakd home, and
`tests/fixtures/config-scenarios/` holds complete scenario homes. Each is a
`config.toml` plus one flat template file per agent under `agents/`. They are
opt-in copies, never loaded automatically. To use one, run
`plakd config show | head -1` to learn the home, copy the scenario's
`config.toml` and its `agents/*.toml` beside it, and `plakd login` to the
accounts it names — templates name accounts only, and no credential ever appears
in these files. A project may also carry templates under
`<project>/.plak/agents/`, where a project file wins over the user file of the
same name.

The shipped default — an empty `[roster] working_set` and no `[root] agent` —
remains in force when no config file exists, so these scenarios never change
default behavior. `bin/plakd/tests/integration/product_config_fixtures.rs` loads
every scenario directory offline, so a schema change breaks the examples rather
than silently rotting them. Each scenario's `config.toml` opens with a comment
that names it and describes the configuration; the same test requires that
name and refuses a scenario pinning `gpt-5.3-codex-spark` or `gpt-5.4-mini`.

## 5. Known flaky classes

Each shipped fix removes the environmental assumption rather than weakening
the assertion. Open investigations name their current containment explicitly.

| Class | Cause | Fix in force |
| --- | --- | --- |
| Symlinked temporary root | macOS `/tmp` and `/var` are symlinks; SQLite opens with `SQLITE_OPEN_NOFOLLOW` and the credential walk refuses symlinked parents | A physical per-checkout `TMPDIR` outside `HOME` (`/private/tmp/plak-test-*` on macOS) from `tools/gate/test-env.sh`, and required for `just nextest` |
| Inherited git config | Fixture git read the host's global config; a global `commit.gpgsign` spawned `gpg` per fixture commit, which failed with ENOMEM under gate load | Test entry points export an empty `GIT_CONFIG_GLOBAL` under `TMPDIR` and `GIT_CONFIG_NOSYSTEM=1` (§1); fixtures set their own identity |
| Fixed-port OAuth loopback | Login tests bound the pinned callback ports, which collide across parallel runs and with a live login | Tests bind an ephemeral port through the managers' loopback seam; the pinned ports are refused in tests (§2.3). The `native-auth-serial` group still serializes the shared process-local fixtures |
| CLI contract interference | Process-level binary contracts collide when run in parallel | The `cli-contract-serial` group |
| Child-start deadline under load | Crash matrices relaunch the current test binary and wait for durable cut markers; CPU and SQLite contention can exceed a bounded start deadline | Durability and install (§2.3) use host-scaled pools of at most sixteen slots, separate from each other and the serial budget lane. Install runs the internal-worker campaigns exclusively at late priority and isolates the signature-sensitive closed-cell campaign. The 240-second slow timeout and explicit matrix watchdogs are unchanged. Child reply and exit watchdogs are hang bounds, not timing assertions |
| Open: closed-cell retirement twin signatures under load | `crash_sweep::durability_ledger_retirement_closed_cells` has produced different per-store commit signatures under competing load; twin-pilot determinism is itself the invariant, so isolation does not establish a root cause | Whole-pool reservation at priority 100 remains containment, not a correctness fix. A bounded probe with four CPU busy loops passed three campaign invocations, each with matching 109-publication twin pilots and 32 representative cuts; it captured no divergence and cannot classify harness timing versus product ordering. The harness deliberately varies answer-before-idle versus drained-idle delivery and gates quit on the subsequent idle observation. Investigate both retained `commit-trace.jsonl` pilots on reproduction; stop and report product-ordering evidence rather than change isolation or the oracle |
| Wall-clock budgets read as contention | Interactive PTY budgets, the 250 ms yield floor, lease and cancellation windows are measurements on an idle machine | The same serial budget lane, assertions unchanged |
| Scripted response windows | A sleeping model can finish before the test's submission, mail claim or crash cut under load | Offline scripts use explicit registry gates for in-process streams and file-sentinel gates for process cuts. Gates preserve cancellation; park predicates register tree notifications before inspecting state |
| Probe peer teardown | A successful TLS 1.3 probe drops its connection before the peer flushes session tickets, so a fixture can drop its listener before the next probe | The byte-count peer disables tickets and gates receipt of the client's Finished until probe teardown; both HTTP ALPNs still require exactly zero plaintext bytes on two fresh connections |
| Paced HTTP upload tail | A client flush finishes before the peer drains bytes already accepted by TLS/TCP; sleeping after every peer read can spend the response-head allowance on that buffered tail | The steady-write law drives the real HTTP/1 dispatcher and request-I/O accounting over a bounded in-memory stream with paused time. Its 8 MiB upload spans more than a second of virtual time in sub-idle steps; a reply just before the unchanged 500 ms post-flush head deadline succeeds, and withholding it expires at that deadline with the original phase/transmission facts. This law runs in the parallel lane, not the wall-clock serial lane; physical TLS coverage remains in the transport integration suite |
| Exchange deadline spent on setup | A short absolute exchange deadline includes resolution, connect and TLS. A wall stopwatch started before execute includes setup. A 30 ms write idle can be spent by scheduling the write itself | These tests start with paused time. A blocking hold inhibits auto-advance until the peer has admitted the request, or until TLS is up for the idle-bound body. The absolute and head budgets then advance on the frozen clock. The HTTP/2 body pauses stay real after the clock resumes, so paced consumption still has to outlive the idle and a stall still has to expire inside 2s. Phase, kind, transmission, and idle-bound assertions are unchanged |
| Live writer mistaken for a dead one | A probe read can time out while the single-threaded server is still slow to serve it; if serving a request stops the server, a retry finds it gone and reports a dead writer | Only `/stop` shuts the server down. The probe retries connect failures while the writer pid is alive and reads one accepted response with a 15 s hang bound. Process exit still fails with the same text. A live process that never answers is a separate hang, not a dead writer |
| Cancel-cell wrapper deadline | The scenario wrapper covers agent setup, the real process start, the cancel and shutdown. On a loaded machine that work reports `deadline has elapsed` before the settle result | The wrapper is a 60 s hang watchdog. The assertion that the running nested command joins within 3 s is unchanged |
| Hosted projected totals | A host's bounded projector pass can defer its view update under load | Counter fixtures explicitly settle the view before asserting counts and rows; a refused-pass regression requires the same counts after catch-up. Presentation tests drive the after-ack pass without the live loop's wall-clock bound, which only defers it |
| Follow-up completion mistaken for the previous turn | A JSONL waiter rescans retained records and matches an earlier `assistant_done` | The refused-fold continuation waits for two completed turns before quit |
| Random retry fits a supposedly shorter deadline | Equal jitter makes the first backoff 500–1000 ms, so an 800 ms pre-dispatch fixture can unexpectedly retry | The fixture deadline is below the minimum backoff; a pure `next_delay` test pins both jitter extremes |
| Nested executor during root interruption | The async turn driver reaches descendant cancellation through a synchronous marker that constructs an application executor | The marker and its callers await the existing async cancellation path; a direct async-context regression requires joined child retirement |
| Retries masking a race | A rerun hides a load-dependent interleaving instead of removing it | No nextest profile carries `retries`; a load failure is fixed with a gate, barrier or notification, never a retry or a wider window |
| Live transient provider failures | HTTP 429 rejection, post-acceptance SSE rate limits, and ambiguous 502 responses are provider outcomes | Live scenarios run zero inference retries; a transient failure is recorded as such and the scenario is re-run deliberately |

A failure in the serial or durability lane, or in any test that spawns real
binaries, pseudoterminals or process groups, is read in four steps. First rule
out the machine: a load average near or above the core count comes first.
Second, run the family alone; these pass in isolation when the tree is sound.
Third, compare failing names across runs: a regression fails the same test
every time, while load fails whichever test lost the race. Fourth, if it still
looks real, revert the change on a scratch copy and rerun under the same load.
This is not permission to ignore a failure: a family that fails in isolation,
or fails the same test twice, is a defect.

## 6. Review focus

Automated gates do not replace judgment. A review asks:

1. Does the change violate the Plakous/plakd layer or ownership boundary?
2. Does it add a peer vertical before the protocol kernels can host a plugin?
3. Is the paid and auth fail-closed posture preserved, with tests updated?
4. Do offline tests cover the behavior change?
5. Is a live path needed, and was a live scenario run intentionally?
6. Is growth intentional capability rather than accidental duplication?
7. Does recovery rest on the current format epoch and the recovery-scope
   table, with build identity as provenance only, working obligations failing
   closed, and semantic change flowing through explicit fork or rebind rather
   than resume?
8. Is every claimed API in an explicit supported inventory, with publication and
   nameability kept distinct from qualification and compatibility?

Secrets, authorization headers, private session data, and unsanitized provider
fixtures are never committed. Live Platform and subscription paths are opt-in,
fail closed without authority, and live in dedicated test utilities and product
paths — never in the default workspace green bar.

Offline provider tests run the real adapter over synthetic TLS when it is the
boundary under review. The product OpenAI boundary admits only its normalized
official origin and scoped reference; the Platform registry proves literal
schemas and `strict: false` on the wire, not merely that a binding constructs.
Responses deadline coverage joins a stalled stream and connection without
authorizing an unfinished tool call. Messages refusal coverage must agree
across streaming and complete canonical assembly, preserve usage/reason,
and authorize no refused proposal or invented assistant output after reopen.
Ordinary empty success and contradictory terminals stay invalid. OpenRouter
coverage distinguishes HTTP rejection from in-band error after acceptance:
one physical inference, prefix before the terminal error, no completion or
extra poll result; attribution headers are exact or absent by route. Tool
choice and tool support are separate admission facts. None of these fixtures
qualifies a live account or model.

Native Claude attribution uses stable entry UUIDs, exact logical prompt and
provider continuation IDs, field order, byte-range attestation and independently
derived UTF-16 suffix/checksum vectors. Keep literal native-executed request
bytes separate from source-derived edge cases; an implementation agreeing
with its own verifier is not an oracle. Fixtures cover first dispatch, tool
continuation, user follow-up and exact reopen, while excluding foreign,
ambiguous or imported prior responses. Provider refusal causality is not
established. Unified patch losslessness tests likewise use exact bytes:
unterminated hunks refuse before any write; explicit no-final-newline
markers preserve content with or without their own final LF.

### 6.1 Identity, finalization and codec regressions

| Family | Evidence law |
| --- | --- |
| Credentials | A synthetic profile GET hydrates a provider UUID without a new login. Hydration/refresh/reopen preserve identity and fence stale writers; malformed, missing and conflicting evidence refuses. Concurrent managers cannot block the executor while reading or replacing shared credentials; account, installation and session identities stay distinct. |
| Finalization and codecs | Shutdown attempts remaining stages and exact release after an earlier dependency panic; fatal cleanup does not masquerade as a refused model command. Canonical codecs round-trip each closed typed capacity, store and tool failure, reject unknowns, and reopen the same result without preparation or dispatch. |
| Admission reserves | Both stores enforce exact ordinary, settlement and recovery bounds. Ordinary refusal is definite before effect admission; already dispatched work may settle in reserve. A refused terminal settlement needs recovery and can consume only its own reserve. Repeats are inert; under-limit refusal does not prevent deletion or reclaim. Test doctor and CLI from observable typed classes, not backend failure text. |
| Custody and deletion | Memory and SQLite run the same custody laws and seeded transition corpus. Only the exact prior holder/fence and independent evidence allow takeover; missing lock files alone prove nothing. A second face must refuse with the typed owner error and write nothing. Clean quit releases every held face. Withdrawal removes exact seeded fork state only; published forks keep their authority. Deleting a root cannot touch a child while the root is still owned. |
| Lines and carry | Line head and generation publish atomically; old heads refuse even with a live lease. Branch-scoped jobs transfer only to the succeeding head/owner, with terminals and numbers preserved. Only the current head records/folds exact job terminals; duplicate receipts are inert. Carry requires a valid quiescent source and target binding, never creates an empty intent runtime, and after an interruption converges to one successor or an unchanged/withdrawn intent. |
| Line conformance | Run one ordered corpus against memory and fully verified SQLite. A predecessor's commit, prune or second rebind refuses as `NotLineHead` without moving the head, even under a live lease. The driver self-fences after that refusal from commit or prune and makes no further store calls. An incorrect terminal fold refuses as `LineTerminalsNotFolded` without changing the head; same-store carry from a Working head refuses as `CarrySourceNotQuiescent`. Hosted carry refuses as `Ownership` under a lease-only holder and `NotQuiescent` for an unfinished turn. |
| Line SIGKILL cuts | Cut rebind at `fork-before-commit` / `fork-after-commit`, carry at `carry-before-commit` / `carry-after-commit`, and lease-only terminal recording at `record-terminals-before-commit` / `record-terminals-after-commit`. Fully verified reopen has one committing head: the generation-1 source with no target, or the generation-2 successor with the source fenced. The terminal record is wholly present or absent. |
| History retention | Reclaim preserves the exact retained ancestry, counters and typed receipt window; discarded checkpoints/receipts are `Pruned`, not corrupt. Cut-tests reopen either exact unpruned or exact pruned history. Working-head prune requires owner assent, retains the bounded terminal window and cannot run while a paid fold awaits its receipt. Child recovery consumes feed facts before deciding which retained head to open. |
| Prune SIGKILL cuts | Cut at `prune-after-enqueue`, `prune-before-commit` and `prune-after-commit`. Fully verified reopen must see the exact unpruned or pruned store, not an intermediate state. |
| GC and inspection | GC applies the feed before pruning, skips live/recovery/workflow authority and finishes eligible archived deletions. Pending workflows block GC and export until settled or withdrawn. Reopen must derive one compaction row even if the old projection was behind; read-only inspection uses the live recovery predicate without writing. |
| Compaction queue | Summary-specific evidence distinguishes a paid fold from ordinary turns. At a cut, an accepted item survives in the paused queue and a draft without durable admission does not. Resume may authorize a new workflow, not a second dispatch of an uncertain one. Endpoint loss preserves the exact item and a typed queue remedy; that fixture is not a claim about ordinary reopen. |

### 6.2 Lease admission regression evidence

Grant validity is checked on the store authority clock at the actual admission
boundary, strictly before expiry, not inferred from a heartbeat/response time.
Drive dispatch after delayed ACK, exact receipt recovery, stale renewal,
failed clock reads and lease expiry without a heartbeat. Refusal preserves the
recorded receipt and makes no prepared start. Tools and evaluators recheck
after waiting for a scheduler permit and before entering the executor; a
lease, cancellation or policy refusal authorizes no effect. A fake evaluator
entry witness checks the runtime callback contract, not placement inside each
interpreter; those placements are separately reviewed and exercised offline.

Public cancellation joins a pending compaction start or stream once and never
dispatches after the cancellation wins, even with a delayed store ACK.
A successful held fold retains accepted follow-up, prompt and steering records
in their specified order under both start and streaming schedules. Offline
prepared-start counters are not provider, billing or network evidence.

A root turn has no wall-clock ceiling (chapter 01 §3.3). Paused-clock tests
allow long ordinary turns, end a non-finishing composition at the operation
round cap after wrap-up, and restore the cap originally admitted without
a second wrap-up or a retroactive configuration change.

### 6.3 Tree folds, reopen and measurement

- **Intent publication.** First recording fixes scope and input. A refused
  consequence restores the acknowledged pre-fold view, without resurrecting
  released process-local ownership or dispatching a seal or hook. Once the
  consequence is acknowledged, retry or reopen cannot repeat its effect.
  Distinguish a saved intent, use/acceptance and an applied intent. In
  particular a live-only accepted steer publishes no revision and only
  rebuilds after application.
- **Row authority.** Live, clear-and-replay, and cold-reopen views agree.
  Apply a retained page only against its cursor or refold it with its retained
  seed; a stale page cannot overwrite later retirement. Repeated reads must
  be inert, with read/refold accounting only for new facts. A deleted fold
  refuses live writes and rebuilds from saved witnesses, not from inferred
  terminal/cache state. Cross-agent applier-diff and seeded multi-child
  corpora prove that unrelated rows, immutable inputs and revisions stay put;
  debug-build diff checks follow the same rule.
- **Effects and results.** Mint, seal, adopt, conflict, retire, cancellation
  and mail have separate exact scope and intent identity. Changed generation,
  stale or foreign caller terminal, and failed consumption cannot mutate the
  destination. A Finalize replay starts at the accepted command boundary,
  consumes a saved consequence once, retains failure/diagnostic or exact
  submitted output, and cannot repeat a hook, seal or result-mail milestone.
  Parent/child failure propagation must reach the same result in either
  projection order. Indexed cuts also cover an intent without its effect.
- **Reload and inspection.** Reload rehearsal uses an isolated fixture home
  with WAL backup and a scrubbed environment; no secrets or holder locks are
  copied. Refusal or signal cleans staging; a live-session race has a typed
  retry remedy distinct from other failures, without printing path, SQL or
  config. Pruning concurrency distinguishes legacy holder/SQLite locks, the
  current product writer and unrelated runtime readers/writers. Inspector
  tests establish decoded-head predicates and display bounds, not live
  credential/evaluator construction, actual child recovery or paid execution.
  A production rehearsal is a separate operator action against backups.

`just dev coverage` runs the parallel and serial nextest lanes with
`cargo-llvm-cov` in a separate target and writes HTML/JSON to `target/llvm-cov/`.
Supply matching executable `PLAK_PY_HOST`, `PLAK_JS_HOST`,
`PLAK_MCP_SERVER` and `PLAK_TEST_BINARY`; missing or non-executable fixtures
refuse. Weighted-lane trials derive the same paths from Cargo artifacts.
Coverage is informational. Product behavior reachable only in another
process belongs in the spawned suite, not a mock built for coverage.

**SQLite reclaim and admission budgets.** Count-based tests exercise
independent priority lanes, 32-job aging and usage publication without an
actor dispatch. An independent mark/sweep oracle and pre/post GC SIGKILL cuts
pin ancestry, exact counter and queue atomicity, and full object verification.
The real actor's admission budget compares statement and VM-step counts over
small and large stores; no full scans, automatic indexes, payload reads,
recursive closure walks or aggregate sum are permitted. The prepublication
path is renew then commit, with at most one bounded reclaim before that pair
under pressure. Branch lifecycle reads must not materialize full heads.
`perf_store --large-store-admission`, `perf_store --branch-reads`, and ignored
tree-fold/close and retention benches are opt-in measurements, not timing
gates. Repeated close and idle/deleted-only folds must not clone or write
unrelated work just to re-establish an existing state.

### 6.4 Operator console and retained-runner evidence

| Family | Evidence law |
| --- | --- |
| Client protocol boundaries | Standalone clients use the exact enrolled protocol, schema and typed bridge-unavailable refusals. Host wire restart matrices drive real sockets and authenticated WebSockets: advisory stop/drain/restart records precede EOF; aborted preparation returns to running. Discovery refuses absent, starting, stopping, incompatible or unverified authority without mutating it. External client/application review is separate; no client repository or runnable UI is a gate input. |
| Accounts and consent | Read-only status distinguishes settled history, pending refresh and unknown attribution. Write-only keys/challenges are redacted even in subprocess diagnostics. Consent binds exact principal/connection/generation/action, expires and cannot burn quota on refused admission. Login attempts obey per-account and host bounds; disconnect retires them. The effect witness becomes sticky only when recovery/dispatch authority crosses the boundary, not when malformed paste or failed preflight refuses. |
| Files and Changes | Session-free reads do not create runtime state. Exact bytes, digest/revision-bound cursors, runner generation and ownership prevent cross-target reads and stale writes. No-follow and product/XDG overlap refusals precede I/O. Traversal, whole-file hash, pending result and BuildRead budgets charge the real work; omissions are flagged, not a clean result. Failed reads preserve retryable cursors, successful reads consume them, and dropped calls release guards. |
| Configuration and profile publication | Etag/exact-byte checks preserve comments and refuse stale forms, invalid values and forbidden writes without mutation. Write/sync/link/rename cuts expose old or complete new bytes; after publication, response loss is not proof of no effect. Use now follows held/remote moves with preserved provenance; only a proven local attachment bootstraps defaults. Missing provenance fails closed, and explicit remote default writes require confirmed etags. |
| Terminal runner | Quotas, exact offsets, owner/generation checks, binary bytes and bounded rings hold with input/ack loss and output backpressure; refusal cannot enqueue, and loss never replays input. Independent streams have fair, bounded credits. Real PTY serial tests prove job control, raw echo/resize and joined cleanup; death and disappearance are distinct from acknowledged unknown loss. Durability cuts preserve live pins across host loss and never signal a remembered PID after runner replacement. Platform kernel observations, not guessed PID reuse, authorize cleanup. |
| Terminal host | Live pins and residency block reclamation, including after reconnect/reconciliation and independent of receipt/namespace exhaustion. Principal/controller authority is private; takeover requires confirmation. Host loss or replacement must preserve precise loss/pin semantics, not create agent job/line records. Both serial PTY and durability cuts are outside offline parallel. Client-side scheduling/browser behavior is separate from host rate-limit, stream order and web policy checks. |
| Retention-only ownership | Frozen v1 fixtures and v2 policy independently check exact verbs, encodings and refusals, including discovery of the previous protocol and rejection outside the supported window. A foreign/stale tuple, insecure parent, peer mismatch or inode change is not authority. Transfer never adopts unsettled foreground work. Multi-build serial fixtures exercise direct delegation, one-time input and terminal consumption, old-controller fencing and no shadow capacity charges. |
| Retention loss | Durability cuts kill the actual original or receiving daemon and lose an acknowledged transfer reply. Preserve exact receipts without input replay, distinguish missing/unreachable old authority from proved death, and never adopt or kill a surviving child using vanished authority. Expiry uses the original lifetime and terminal time, not the first observer's, and never stops a running child. |

Install owns the host upgrade family. Publication SIGKILL cuts belong to
durability; injected parent-sync failures run offline. Wall-clock discovery
bounds belong to serial with matching offline exclusions. See §2.2 and
§2.3 for the complete lane rules.

### 6.5 Opt-In MCP OAuth Interoperability

`just live mcp-oauth` (`tests/live/mcp_oauth/`) is a local, zero-inference
interoperability lane, not a gate or a substitute for owner-account/provider
qualification. Digest-pinned Compose images run Keycloak 26, strict
node-oidc-provider RFC 8707 resource indicators, an official-SDK streamable
HTTP MCP fixture, and pinned Playwright browser approval with disposable test
users. plakd is built with `test-support` and driven by `PLAK_TEST_SCRIPT` in an
isolated private home with a separate Unix-socket gateway per case; no
operator subscription store is inherited. The JSON
report binds the binary hash and source revision to per-scenario CLI/session
log assertions and independent AS dispatch/MCP audience and effect witnesses.
Browser URLs, authorization codes, device codes, tokens and client secrets
are not retained in the report. Teardown removes the home, test grants and
Compose resources, including on failure. A state-key carry scenario requires
an explicitly supplied second test-support binary with a different state key;
missing evidence is reported as blocked, never a pass. Resource-matching
expectations are external JSON data so the lane can follow the owning MCP
specification without changing its fixture logic. A separate loopback origin
proxies token, device and revocation endpoints advertised by verified issuer
metadata; safe boolean witnesses prove confidential secret delivery. Removing
the stored revocation endpoint from that metadata must refuse logout before
secret dispatch. Browser paste-back rows accept `session_state` plus another
extension and refuse duplicate `state` before token issuance. Paste-back
fixtures omit the optional device endpoint so remote device preference cannot
silently replace their callback coverage. Only offline parser and
redaction self-tests run in the tooling group; no gate starts Docker, a
browser, or these authorization servers. Linux runs use a native Docker host;
macOS requires Docker Desktop host networking for the loopback URL identities.

Compiled-review time is never a qualification precondition: `just full`'s
package group reports remaining days without failing. Publication dry runs
and uploads separately require more than a third of each compiled review
window remaining (chapter 06).
