# plakd / Plakous

This monorepo is **Plakous**, an embeddable single-agent multi-provider harness
library, and **plakd**, the host server and operator/batch CLI that consumes only the public `plakous`
facade and the host-layer crates above it. Plakous owns the loop, providers,
profiles, auth, and admit/revalidate; plakd owns product policy and wire UX. There is no second
runtime in plakd and no multi-agent topology in Plakous L0 — L0 is exactly
`plakous-model`, `plakous-tools`, `plakous-agent` and the `plakous` facade.
The workspace version is 0.0.4. Supported and published-internal packages are
publishable to crates.io as one exact cohort and repository-only packages are
not (spec 06 §1); uploading is a maintainer act, and a source checkout is a
complete build.

Host/web serving is supported in the default product. The public stdio bridge
owns discovery and explicitly opted-in local startup; bare `plakd` prints help
without opening a session or starting a host. There is no default plakd client
or UI in this repository: bring and maintain your own client. Product wire DTOs
belong in `plakd-wire-types`; handlers, credential resolution, settings
metadata and runtime/store authority stay in plakd.

## First reading

`docs/spec/` is the authority for current behavior; `docs/user-guide/` is the
authority for usage. Read `docs/spec/00-overview.md` first, then the chapter
that owns what you are changing: `01` Plakous, `02` plakd, `03` orchestration,
`04` security and boundaries, `05` testing and evidence, `06` build and support
surface, `07` configuration reference.

The tree carries no decision history. Commit messages and external notes are
history, never prerequisite reading, and never cited as authority.

## Dev

MSRV and pinned toolchain are Rust 1.100.0; supported platforms are Linux x86_64
musl and Apple Silicon macOS. Run `just dev-tools install` once for the pinned
cargo-nextest, cargo-deny and ruff and the `wasm32-unknown-unknown` rustc
target the `standalone` group checks the portable crates for; an existing setup
re-runs it to add that target. The tools check every tier runs first also
requires a working jj: `PLAK_TEST_JJ` when set, else `jj` on `PATH`, else the
newest pinned jj that install unpacks under `.dev-tools/jj`.
`tools/gate/dev-tools.pins` holds every tool version, URL and checksum; a cloud
agent VM installs them from `.agents/setup`. Worker branches iterate and finish
with `just fast <packages>` plus the groups their change needs. The integration
batch, install and publication gates are spec 06 §8.
`just musl container` is a maintainer-only container gate: it needs the preloaded
offline image `plak-qual-base:86ab9e67`, which is not published (spec 06).

```text
just fast [--rdeps] [PKG...] # static and changed-package parallel tests
just batch                # standalone lint, contracts, tooling and scoped integration tests
just batch-mac            # Mac workspace clippy and platform-sensitive tests
just install              # Linux install supplement: builds, rustdoc and install tests
just install-checks       # exact-tree Mac install supplement: full offline suite
just steps GROUPS         # exactly those comma-separated groups, in canonical order
just full                 # every group, including archive verification and long soaks
just fmt                  # cargo fmt, then ruff format on the checked Python paths
just clippy               # workspace all-features clippy, warnings denied
just nextest ARGS         # pinned cargo nextest under the isolated test environment
just publish [--execute]  # crates.io cohort dry run; --execute uploads
just dev-tools ARGS       # pinned cargo-nextest, cargo-deny and ruff; `deny` is the dependency audit
just build PROFILE        # debug, release, maintainer or profiling; no snapshot and no PATH change
just install FLAVOR       # release (plain) or maintainer (dogfood) immutable snapshot on PATH
just musl native          # complete Linux x86_64 musl coverage (use --batch for integration)
just musl container       # full tier in the pinned offline Alpine image; maintainer-only
just live RECIPE          # smoke, soak, jobs, mcp-oauth, census, estimate, daily-driver, qualify
just perf RECIPE          # smoke, samply, flamegraph, stat, bloat, density, check; not a gate
just prompts RECIPE       # render, diff, pins, native-compare
just dev RECIPE           # home-install, home-reset, reload-rehearsal, coverage, test-timing, crash-sweep, crash-compare, jj-compat, weighted-lanes
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

All tiers use `tools/gate/dev-check.sh`. Fast keeps format and structure checks
global and narrows clippy, feature-profile compiles and the parallel lane by
package; the test build uses the shared all-feature workspace resolution.
Set `PLAK_BATCH_BASE` to the commit before the integration batch. Linux batch
uses the changed packages' reverse dependencies plus runtime-binary edges
for parallel tests; Mac batch intersects that scope with the platform-sensitive
filters. Linux batch overlaps standalone, package-lite and public compile
contracts with test preparation, never with tooling or timed test lanes.

Production install uses exact-tree batch evidence plus the install groups,
full Mac suite and strict crash sweep (spec 06 §8). Full adds release-only
variants, standalone rustdoc, archive verification and ignored retirement
soaks. A passing full run on a clean tree records the evidence publication
requires. Inert documentation changes can reuse it; a `README.md` change
needs new archive verification (spec 06 §6.5). Linux batch runs install
families triggered by its base diff, or all families when no base is set.

The nextest profiles partition the offline suite. Edit the owning budget or
crash filter (install families are in `.config/nextest-install.toml`), then
run `python3 tools/gate/nextest_layout.py --sync`. Public compile contracts
keep a persistent cache beside the gate target (`PLAK_PUBLIC_CONTRACTS_DIR`
overrides it).
Hand-run nextest is `just nextest`. It exports the isolated test environment
from `tools/gate/test-env.sh`: per-checkout `TMPDIR` (`tools/gate/test-tmpdir.sh`,
`PLAK_TEST_TMPDIR` overrides), `PLAK_HOME` under that directory (mode 0700), an
empty `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_NOSYSTEM=1`, and it unsets the
provider API keys. The gate, coverage, the crash sweep, perf smoke and the live
drivers use the same helper; the live drivers' `--json` form keeps the keys
(spec 05 §1).
There is no CI: the only workflow,
`.github/workflows/release.yml`, builds, attests and releases a pushed `v*` tag, and a manual run on `main` is the pre-tag dry run that releases nothing
(`tools/release/package-release.sh`, `install.sh`; spec 06 §6.6).

Transport faults for smoke and soak runs: a `test-hooks` build of `plakd` reads
`PLAK_TEST_FAULTS`, e.g.
`host=api.x.ai,path=/v1/responses,nth=2,fault=429,retry-after=5;host=openrouter.ai,fault=stall,seconds=40,after-events=3`,
and injects statuses, connect failures, and mid-stream resets, clean EOFs and
stalls into real provider traffic. Omit `host`/`path` and discovery or token
requests count too. Syntax and classification: spec 05 §2.8.

Prompt renders are regenerated locally into the gitignored `target/prompt-renders/`
and are **never committed**; correctness is pinned by sha256 tables in code.

The workspace has 43 packages in three classes: supported (5),
published-internal (35), repository-only (3). Publication and nameability
confer accessibility, not support.

Packages declare the dependency features their Rust targets use rather than
relying on another workspace member's feature unification. Native routes
(`codex`, `grok`, `claude`), API exports, evaluators,
MCP and local page fetch are independent at the facade boundary. Persisted route
and tool vocabulary stays readable even when its runtime is omitted.
The feature checker compiles eight library profiles without workspace feature
unification: facade none, `full`, `composition`, `claude`, `openai` and
`gateway`; hosted none and all-features. It
requires locally declared UUID generation features, including for test code.
It validates 2n+3 feature closures per package (none, each singleton, full
minus each feature, default and full), with a reviewed cap of 24 public
optional features. Reduced products compile only in the full-only `variants`
group, not in batch.
`check_features.py` owns the sole named-variant table. Only the default product
is shipped and supported; API and minimal are built and tested only.
The root workspace excludes `.plak` operator state and fork worktrees; the
workspace checker requires this exclusion without permitting unreviewed ones.
The workspace checker also ratchets, per package, item-level allows of
`clippy::too_many_lines` and `clippy::too_many_arguments` and files over 1500
non-test lines (`[package.metadata.plakous-lint]` in the package's own
manifest, spec 06 §4): a split that removes debt lowers its ceiling, and no
new crate-wide allow of either lint is permitted.

Invariants the spec states — single-owner driver with one terminal result per
admitted root turn, single-agent L0, exact-URL auth that never falls back across
routes, single-dispatch paid compaction — are settled. Do not re-litigate them
in a change; amend the chapter that owns them.

## Working agreements

- Do not preserve backward compatibility. Remove obsolete paths instead of
  adding compatibility layers, fallbacks, or migrations. Unshipped prototype
  APIs and databases have no compatibility claim.
- The first exception is cross-build session reference: the export document is the
  cross-build session carrier. A build MUST import export documents of its own version
  and of the immediately previous version. Any change to the document,
  including the embedded canonical-history and binding encodings, bumps the
  version; the previous version's importer is kept for one bump only. Older
  builds need not import newer documents.
- The second exception is the runner job-retention endpoint: a runner daemon MUST
  serve, and the next build's daemon MUST speak, the retention endpoint of its own
  version and of the immediately previous version, so that jobs started by the
  previous build survive a planned upgrade. The endpoint is a same-user,
  same-machine private socket with a fixed verb set (lines, transfer, jobs
  snapshot/watch, read/poll, write, stop, disown, ack); it never starts work,
  performs workspace effects or replays effects. Any change to its verbs or
  encodings bumps its version; the previous version's speaker is kept for one
  bump only.
- The third exception is the credential gateway protocol: a gateway MUST
  serve, and the next build's callers MUST speak, the gateway protocol of its
  own version and of the immediately previous version, so that a planned
  upgrade of either side keeps inference and account operations working. Its
  endpoints are fixed and path-versioned; it relays one upstream exchange per
  request, never retries, translates or fails over (a reused idle upstream
  socket that fails with a protocol error before its TLS stream accepts any
  request plaintext may be replaced once within that exchange, with the peer
  receiving at most an incomplete request prefix), and grants no session or
  store authority. Any change to its endpoints, encodings or binding facts
  bumps its version;
  the previous version's speaker is kept for one bump only.
- The fourth exception is in-place store revisions: a build MAY upgrade a
  store in place when the change only adds tables, indexes, views, or nullable
  or defaulted columns whose default means exactly the old behaviour. Each
  revision runs in one transaction on one store, reads no other store, follows
  a copy of the stopped store, and is never reversed; an older build refuses a
  newer revision without writing. A change to authority, lineage, history
  encoding or binding digests bumps the store's generation instead, which
  changes the state key and uses settled carry. Revisions accumulate within a
  generation and are deleted when the generation bumps.
- Choose the simplest implementation that fully meets the current architecture.
  Avoid speculative abstractions, configuration, and indirection.
- Grow in layers: smallest end-to-end product first, then add capability on a
  product that already works. Never trade a working product for unfinished work.
- Keep visibility as narrow as possible (`private`, then `pub(crate)`, then
  public). Keep parsing and presentation at the product boundary; neutral core
  types must not depend on CLI or client presentation, and provider names must not enter
  `plakous-model` or `plakous-agent`.
- Prefer established libraries already in the tree over reimplementation.
- Optimize for the reader: explicit data flow, small modules, early returns,
  enums for distinct states, typed errors with sources. Comments explain
  non-obvious invariants, not control flow the code already shows.
- Test externally meaningful behavior and failure modes, deterministically. The
  daily bar is unit → offline integration; live e2e is opt-in and paid.
- Product host changes need the tested maintainer snapshot and matching
  sidecars. Keep a paired host and runner on the same source revision; the
  runner uses a dedicated command link that normal installation does not move.
  Settle work before a coordinated upgrade and report host and runner binary
  IDs. An agent must not interrupt a running session or install merely to
  review a refactor.
- A change to public semantics, crate boundaries, persistence or wire formats,
  security boundaries, or supported platforms MUST update the owning
  `docs/spec/` section in the same change.
- No credentials, auth headers, private session data, or unsanitized provider
  fixtures in the tree, logs, or tests. The workspace forbids `unsafe`.
