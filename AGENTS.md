# Plak / Plakous

This monorepo is **Plakous**, an embeddable single-agent multi-provider harness
library, and **Plak**, the CLI/TUI that consumes only the public `plakous`
facade and the Plakous crates above it. Plakous owns the loop, providers,
profiles, auth, and admit/revalidate; Plak owns product UX. There is no second
runtime in Plak and no multi-agent topology in Plakous L0 — L0 is exactly
`plakous-model`, `plakous-tools`, `plakous-agent` and the `plakous` facade.
Plak is built from source: no releases, no published binaries, nothing on
crates.io.

## First reading

`docs/spec/` is the authority for current behavior; `docs/user-guide/` is the
authority for usage. Read `docs/spec/00-overview.md` first, then the chapter
that owns what you are changing: `01` Plakous, `02` Plak, `03` orchestration,
`04` security and boundaries, `05` testing and evidence, `06` build and support
surface, `07` configuration reference.

The tree carries no decision history. Commit messages and external notes are
history, never prerequisite reading, and never cited as authority.

## Dev

MSRV and pinned toolchain are Rust 1.99.0; supported platforms are Linux x86_64
musl and Apple Silicon macOS. Run `just dev-tools install` once for the pinned
cargo-nextest and cargo-deny. Run `just fast` or `just changed` while
iterating and `just check` before each commit. Before installing a maintainer
build, run on Linux
`just steps static,builds,rustdoc,tests-parallel,tests-serial,tests-durability,contracts,tooling`
and a strict crash sweep, plus `just platform` on macOS and `just musl-native`
on Linux. `just full` and the full strict crash sweep run at publication
boundaries. `just musl-check` is a maintainer-only container gate: it needs the
preloaded offline image `plak-qual-base:6b7eb083`, which is not published
(spec 06).

Use `just changed [BASE]` for change-scoped verification: it prints the rules
and required groups before running them, scopes both test lanes to package
reverse dependencies or doc/config readers, and widens unknown paths to check.
Reader references the planner cannot resolve also widen. Ordinary package diffs
stay scoped; prose, runtime path templates and root-only literals are not readers.
Gate inputs (the runner, prerequisites, planners, budget selector, nextest config, Justfile,
checkers and their shared policy) widen to check (including tooling) and both checkers.
BASE must be a tree that already passed `just check` on this toolchain. The
default is `git merge-base HEAD main`, or HEAD~ on branch main itself. Missing
main or a merge-base equal to HEAD off main requires an explicit BASE, as does
a `main` with a single commit (HEAD~ does not exist): pass a checked commit.
Forked runs pass an explicit checked fork point, not the host's main.
The plan prints its BASE and warns loudly (without failing) on an empty diff.
Use check for unconditional whole-workspace pre-commit evidence, and full for
gate changes and publication. `just fast` deliberately omits
serial budgets; it is iteration evidence, not a substitute for changed/check.

```text
just fast [PKG...]  # static checks + parallel nextest lane over diff packages and dependents
just changed [BASE] # conservative plan from a checked BASE; both scoped lanes
just steps GROUPS   # exactly named comma-separated groups, in full-tier order
just check          # fmt/clippy, structure checks, feature builds, both lanes, contracts, tooling
just platform       # fmt/clippy, default/maintainer builds, parallel, serial and durability lanes
just full           # check + rustdoc, durability lane, perf smoke, LOC; all groups
just musl-native    # platform tier for x86_64-unknown-linux-musl (static-PIE via rust-lld)
just musl-check     # full tier in the pinned offline Alpine container (publication)
just crash-sweep [--shapes SHAPES] [--jobs N] [--repeat R] [--strict] # opt-in commit cuts
just test           # cargo nextest --profile offline
just fmt / clippy   # formatting; clippy -D warnings
just build-release  # maintainer release binary, symlinked onto PATH as `plak`
just density        # LOC report, informational
just deny           # cargo deny (when deps change)
just prompts / prompts-diff / prompts-pins   # local prompt renders
just smoke [route] / just soak [route] --hours N   # opt-in live runs (tests/live)
```

`eval/` is the opt-in Harbor bakeoff plane: Rung A uses selected
Terminal-Bench 2.1 tasks; Rung B uses Terminal-Bench 4.0 (see
`eval/README.md`). Neither paid Harbor runs nor its zero-inference
adapter tests (`just harbor-test`) are in any gate tier. There is no second
runtime. Vendor comparator arms upload pinned Linux CLI prefixes matching
`HARBOR_ARCH` (`amd64` or `arm64`, defaulting to the Docker daemon's architecture);
they do not nvm/`@latest`/`curl | bash` per trial. When a Claude native-home
arm exists, compare interactive subscription OAuth, not `claude --print`.

All four tiers are `scripts/dev-check.sh` and build the whole workspace with
all features, so they share one artifact set. `just fast` maps the working-tree
diff against HEAD to packages (a root Cargo input means all of them), runs
workspace clippy (only changed crates and their dependents re-lint) and the
parallel nextest lane over `rdeps(...)` of those packages; the serial
wall-clock-budget lane is left to `just check`. `just check` runs static checks with warnings denied,
default-feature and maintainer-feature builds (rustc warnings denied, without
rebuilding the all-features test artifacts), public compile contracts,
offline nextest in parallel and serial budget lanes, and Python tooling self-tests, once on Linux.
It does not run the durability lane. `just platform` runs only fmt, cfg-specific clippy, the same warning-denied
default-feature and maintainer-feature builds, and the parallel, serial and durability lanes per install on macOS/native musl;
it excludes the platform-independent structure checkers and public contracts.
`just full` adds rustdoc with warnings denied, the durability lane, and the
informational perf smoke and source-size report. The nextest profiles are
`default` and `offline`.
Check, full and explicit static groups run both structure checkers
unconditionally after fmt. Fast and changed scope them to their inputs:
workspace checks for Rust source, manifests/lockfiles, tests/fixtures/** or checker changes;
feature checks for Rust source, manifests/lockfiles, toolchain inputs or checker changes.
Gate inputs, unknown paths and unresolved readers widen to both. Without
unresolved readers, docs-only and ordinary tooling-only diffs skip both, with
printed reasons; fmt and workspace clippy still run.
Public compile contracts are in check and full, not platform;
their fixture builds use a persistent `.public-contracts/<target-dir-name>/<triple>`
directory beside `CARGO_TARGET_DIR` (or the checkout's `target`), outside workspace
artifact sweeps and gitignored so lane checkout cleanup preserves it.
`PLAK_PUBLIC_CONTRACTS_DIR` overrides that cache root; the musl container sets it
under its persistent target mount, keyed by the pinned image tag and target triple.
Changed selects them for every Rust source or manifest, fixtures or
unknown paths. The full group inventory is ordered once in `gate_plan.py`:
`static`, `builds`, `rustdoc`, `tests-parallel`, `tests-serial`, `tests-durability`,
`contracts`, `tooling`, `perf`, `loc`.
Every selected group runs even after another group fails; static likewise runs
all selected steps and reports every failed step. The final failure summary
names all failed groups and the gate exits non-zero. Invalid plans and missing
prerequisites still stop before group execution.
`tests-durability` is in platform and full, not in check; `just changed`
selects it only for a package that owns one of its
tests, plus that package's reverse dependencies. Each group appends a timing
sample to `.gate-times.jsonl` beside the target directory and warns, without
failing, when a group exceeds twice the median of its last 10 successful samples
on that platform triple (no warning below 3 samples). Explicit groups do not
imply other groups; Rust test groups still prepare their real-worker prerequisite.
Changed builds default and maintainer features for every package Rust or prompt change,
as check does. Untracked `.rs` inputs are covered by the diff;
ignored `**/proptest-regressions/**` files present locally select their owners.
Reader discovery scans tracked Rust sources except benches/ and examples/.
Non-embedded root-only literals are ignored. Path templates record their literal
leading directory when it exists or is under docs/configs; other nonexistent
prefixes are runtime paths, and prose is not a path. Dynamic roots and unresolved
embedded targets still widen to whole check. An exact source/literal inventory
exempts reviewed runtime IDs, URLs and private-home paths; tests reject stale
entries. Completeness tests
inventory existing quoted docs/config paths and known executable-discovery
markers; artifact coverage includes every package in its Cargo metadata
workspace dependency closure. They do not prove arbitrary dynamic readers.
`dev-check.sh` and the test recipes set `TMPDIR` from `scripts/test-tmpdir.sh`
(a per-checkout 0700 directory under `/dev/shm` on Linux, where fsync under gate
load breaks process deadlines, and under `/private/tmp` on macOS, outside
`HOME`; `PLAK_TEST_TMPDIR` overrides) and `PLAK_HOME=$TMPDIR/plak-isolated-home`
(mode 0700) for nextest, so a test that forgets a product home cannot create the
operator's `~/.plak`. Set both yourself if you run `cargo nextest` by hand:
`TMPDIR="$(bash scripts/test-tmpdir.sh)"`. There is no CI.

Transport faults for smoke and soak runs: a `test-hooks` build of `plak` reads
`PLAK_TEST_FAULTS`, e.g.
`host=api.x.ai,path=/v1/responses,nth=2,fault=429,retry-after=5;host=openrouter.ai,fault=stall,seconds=40,after-events=3`,
and injects statuses, connect failures, and mid-stream resets, clean EOFs and
stalls into real provider traffic. Omit `host`/`path` and discovery or token
requests count too. Syntax and classification: spec 05 §2.8.

Prompt renders are regenerated locally into the gitignored `target/prompt-renders/`
and are **never committed**; correctness is pinned by sha256 tables in code.

The workspace has 39 packages in three classes: supported (5),
published-internal (31), repository-only (3). Publication and nameability
confer accessibility, not support.

Packages declare the dependency features their Rust targets use rather than
relying on another workspace member's feature unification. The feature checker
requires locally declared UUID generation features, including for test code.
The root workspace excludes `.plak` operator state and fork worktrees; the
workspace checker requires this exclusion without permitting unreviewed ones.

Invariants the spec states — single-owner driver with one terminal result per
admitted root turn, single-agent L0, exact-URL auth that never falls back across
routes, single-dispatch paid compaction — are settled. Do not re-litigate them
in a change; amend the chapter that owns them.

## Working agreements

- Do not preserve backward compatibility. Remove obsolete paths instead of
  adding compatibility layers, fallbacks, or migrations. Unshipped prototype
  APIs and databases have no compatibility claim.
- The one exception is cross-build session reference: the export document is the one
  cross-build carrier. A build MUST import export documents of its own version
  and of the immediately previous version. Any change to the document,
  including the embedded canonical-history and binding encodings, bumps the
  version; the previous version's importer is kept for one bump only. Older
  builds need not import newer documents.
- Choose the simplest implementation that fully meets the current architecture.
  Avoid speculative abstractions, configuration, and indirection.
- Grow in layers: smallest end-to-end product first, then add capability on a
  product that already works. Never trade a working product for unfinished work.
- Keep visibility as narrow as possible (`private`, then `pub(crate)`, then
  public). Keep parsing and presentation at the product boundary; neutral core
  types must not depend on CLI/TUI, and provider names must not enter
  `plakous-model` or `plakous-agent`.
- Prefer established libraries already in the tree over reimplementation.
- Optimize for the reader: explicit data flow, small modules, early returns,
  enums for distinct states, typed errors with sources. Comments explain
  non-obvious invariants, not control flow the code already shows.
- Test externally meaningful behavior and failure modes, deterministically. The
  daily bar is unit → offline integration; live e2e is opt-in and paid.
- The final handoff for product work is human TUI review through `plak`: verify
  the changes, install the tested maintainer snapshot (`just build-maintainer`)
  with matching sidecars, and report its binary ID. Do not interrupt a running
  session; a running TUI shows `new build installed · /reload`, and
  `/reload` resumes the session on the new build. Automated
  checks do not substitute for a maintainer's TUI review.
- A change to public semantics, crate boundaries, persistence or wire formats,
  security boundaries, or supported platforms MUST update the owning
  `docs/spec/` section in the same change.
- No credentials, auth headers, private session data, or unsanitized provider
  fixtures in the tree, logs, or tests. The workspace forbids `unsafe`.
