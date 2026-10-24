# Plak / Plakous

This monorepo is **Plakous**, an embeddable single-agent multi-provider harness
library, and **Plak**, the CLI/TUI that consumes only the public `plakous`
facade and the Plakous crates above it. Plakous owns the loop, providers,
profiles, auth, and admit/revalidate; Plak owns product UX. There is no second
runtime in Plak and no multi-agent topology in Plakous L0 — L0 is exactly
`plakous-model`, `plakous-tools`, `plakous-agent` and the `plakous` facade.
The workspace version is 0.0.2. Supported and published-internal packages are
publishable to crates.io as one exact cohort and repository-only packages are
not (spec 06 §1); uploading is a maintainer act, and a source checkout is a
complete build.

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
cargo-nextest and cargo-deny. Run `just fast` while iterating and `just check`
before each commit. Before `just install maintainer`, run `just full` and a
strict crash sweep (`just dev crash-sweep --strict`) on Linux, plus
`just platform` on macOS and `just musl native` on Linux. The full strict
crash sweep runs at publication boundaries.
`just musl container` is a maintainer-only container gate: it needs the preloaded
offline image `plak-qual-base:2f225fd7`, which is not published (spec 06).

```text
just fast [PKG...]        # static checks, then the parallel lane for the diff's packages and dependents
just steps GROUPS         # exactly those comma-separated groups, in canonical order
just check                # pre-commit: fmt, clippy, structure, builds, parallel+serial, contracts, tooling
just platform             # per install: fmt, clippy, builds, parallel, serial, durability
just full                 # every group: check, plus rustdoc, durability and the publish dry run
just fmt                  # cargo fmt --all
just clippy               # clippy -D warnings on all targets
just publish [--execute]  # crates.io cohort dry run; --execute uploads
just dev-tools ARGS       # pinned nextest and cargo-deny; `deny` is the dependency audit
just build PROFILE        # debug, release, maintainer or profiling; no snapshot and no PATH change
just install FLAVOR       # release (plain) or maintainer (dogfood) immutable snapshot on PATH
just musl native          # Linux x86_64 musl platform tier, static PIE via rust-lld
just musl container       # full tier in the pinned offline Alpine image; maintainer-only
just live RECIPE          # smoke, soak, tui-soak, jobs, mcp-oauth, census, estimate, daily-driver
just perf RECIPE          # smoke, samply, flamegraph, stat, bloat, density; not a gate
just prompts RECIPE       # render, diff, pins, native-compare
just tui RECIPE           # story, frames
just dev RECIPE           # home-install, home-reset, reload-rehearsal, coverage, crash-sweep, jj-compat
just harbor RECIPE        # Harbor bakeoff, not a gate; `test` is the zero-inference adapter suite
```

`eval/` is the opt-in Harbor bakeoff plane: Rung A uses selected
Terminal-Bench 2.1 tasks; Rung B uses Terminal-Bench 4.0 (see
`eval/README.md`). Neither paid Harbor runs nor its zero-inference
adapter tests (`just harbor test`) are in any gate tier. There is no second
runtime. Vendor comparator arms upload pinned Linux CLI prefixes matching
`HARBOR_ARCH` (`amd64` or `arm64`, defaulting to the Docker daemon's architecture);
they do not nvm/`@latest`/`curl | bash` per trial. When a Claude native-home
arm exists, compare interactive subscription OAuth, not `claude --print`.

All tiers are `scripts/dev-check.sh` and build the whole workspace with all
features, so they share one artifact set. The groups, in canonical order, are
`static` (fmt, locked dependency fetch, offline first, online only for missing
cache inputs, the workspace and feature structure checkers, clippy -D warnings),
`builds` (default-feature and maintainer-feature builds, rustc warnings denied),
`rustdoc`, `tests-parallel`, `tests-serial`, `tests-durability`, `contracts`
(public compile contracts), `tooling` (Python self-tests) and `package` (the
publish dry run: `scripts/publish.sh --gate` prefetches archive verification
inputs offline first, online only for missing cache inputs, then packages and
verifies the cohort strictly offline; compiled review expiry is informational
here, while publication dry runs and uploads require more than 21 days
remaining). Check runs all but rustdoc, durability and package; platform runs
static without the platform-independent structure checkers, builds and the
three lanes; full runs every group, and a full pass on a clean tree records that
tree as the evidence `just publish` requires.
`just fast` maps the working-tree diff against HEAD to packages (a root Cargo
input means all of them), runs static and the parallel lane over `rdeps(...)`
of those packages; it is iteration evidence, not a substitute for check. Every
selected group runs even after another group fails; the final summary names
all failed groups and the gate exits non-zero. Explicit groups do not imply
other groups; Rust test groups still prepare their real-worker prerequisite.
The nextest lanes are profiles in `.config/nextest.toml`: `offline` (parallel),
`serial` (wall-clock budgets) and `durability` (crash matrices), which
partition the suite; a new serial test goes into the `serial` or `durability`
filter and the `offline` exclusion. Contract fixture builds use a persistent
`.public-contracts/<target-dir-name>/<triple>` directory beside
`CARGO_TARGET_DIR` (`PLAK_PUBLIC_CONTRACTS_DIR` overrides it).
`dev-check.sh` and `just dev coverage` set `TMPDIR` from `scripts/test-tmpdir.sh`
(a per-checkout 0700 directory under `/dev/shm` on Linux, where fsync under gate
load breaks process deadlines, and under `/private/tmp` on macOS, outside
`HOME`; `PLAK_TEST_TMPDIR` overrides) and `PLAK_HOME=$TMPDIR/plak-isolated-home`
(mode 0700) for nextest, so a test that forgets a product home cannot create the
operator's `~/.plak`. Set both yourself if you run `cargo nextest` by hand:
`TMPDIR="$(bash scripts/test-tmpdir.sh)"`. There is no CI: the only workflow,
`.github/workflows/release.yml`, builds, attests and releases a pushed `v*` tag
(`tools/release/package-release.sh`, `install.sh`; spec 06 §6.6).

Transport faults for smoke and soak runs: a `test-hooks` build of `plak` reads
`PLAK_TEST_FAULTS`, e.g.
`host=api.x.ai,path=/v1/responses,nth=2,fault=429,retry-after=5;host=openrouter.ai,fault=stall,seconds=40,after-events=3`,
and injects statuses, connect failures, and mid-stream resets, clean EOFs and
stalls into real provider traffic. Omit `host`/`path` and discovery or token
requests count too. Syntax and classification: spec 05 §2.8.

Prompt renders are regenerated locally into the gitignored `target/prompt-renders/`
and are **never committed**; correctness is pinned by sha256 tables in code.

The workspace has 40 packages in three classes: supported (5),
published-internal (32), repository-only (3). Publication and nameability
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
  the changes, install the tested maintainer snapshot (`just install maintainer`)
  with matching sidecars, and report its binary ID. Do not interrupt a running
  session; a running TUI shows `new build installed · /reload`, and
  `/reload` resumes the session on the new build. Automated
  checks do not substitute for a maintainer's TUI review.
- A change to public semantics, crate boundaries, persistence or wire formats,
  security boundaries, or supported platforms MUST update the owning
  `docs/spec/` section in the same change.
- No credentials, auth headers, private session data, or unsanitized provider
  fixtures in the tree, logs, or tests. The workspace forbids `unsafe`.
