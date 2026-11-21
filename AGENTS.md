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

Rust 1.100.0 is the pinned toolchain and MSRV. Work targets Linux x86_64 musl
and Apple Silicon macOS. Run `just dev-tools install` to install or update the
pinned tools and the `wasm32-unknown-unknown` target; cloud workspaces use
`.agents/setup`. All tiers require working jj: `PLAK_TEST_JJ`, then `PATH`,
then the newest pinned installation under `.dev-tools/jj`.

```text
just fast [--rdeps] [PKG...] # static and changed-package parallel tests
just batch                # standalone lint, contracts, tooling and core integration tests
just batch-mac            # Mac workspace clippy and platform-sensitive tests
just install              # Linux supplement: builds, rustdoc, full parallel and install tests
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
just dev RECIPE           # coverage, crash-sweep, reload-rehearsal and other dev helpers
just harbor RECIPE        # opt-in bakeoff; `test` is the zero-inference adapter suite
```
Finish worker changes with `just fast <packages>` and any affected groups. Set
`PLAK_BATCH_BASE` to the pre-batch commit for integration. Neither batch runs
the install profile. For changes to install-family tests or code they exercise
(including crash-sweep shapes), run `just steps tests-install` before handoff.
See spec 05 §1 and §2.3 for gate selection, lane membership and isolation;
spec 06 §8 for exact-tree integration, install and release evidence. Production
install includes the complete install profile, Mac suite and strict crash sweep.
Publication requires clean-tree full evidence; inert docs can reuse it, but a
`README.md` change needs fresh archive verification (spec 06 §6.5).
`just musl container` is maintainer-only and needs the unpublished pinned image.

Edit the owning nextest budget or crash filter (install families in
`.config/nextest-install.toml`), then run
`python3 tools/gate/nextest_layout.py --sync`. Use `just nextest` for manual
tests under the isolated environment (spec 05 §1). Transport injection needs
a `test-support` binary and follows spec 05 §2.8. Prompt renders are regenerated
into gitignored `target/prompt-renders/` and are **never committed**; update the
in-code sha256 pins when reviewed prompt output changes.
There is no CI; the tag release workflow and manual dry run are in spec 06 §6.6.
Harbor and its adapter tests are outside all tiers. Follow the pinned
architecture-matched vendor install and interactive subscription OAuth rules
in `eval/README.md` and spec 05 §1.2; do not use `claude --print` for native-home
subscription comparisons.

Declare dependency features (including test UUID generation) at the point of
use; do not rely on workspace feature unification. Keep facade selections
independent and persisted vocabulary readable with routes/tools omitted;
only the default product build is supported. Follow the closure checks and
reviewed feature cap in spec 06. Keep the root workspace exclusions for
`.plak` state and fork worktrees; do not add unreviewed exclusions.
Ratchet per-package item-level `clippy::too_many_lines`,
`clippy::too_many_arguments`, and files over 1500 non-test lines in
`[package.metadata.plakous-lint]`: lower the ceiling after removing debt, and
do not add a crate-wide allow for either lint (spec 06 §4).

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
