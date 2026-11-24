# plakd / Plakous

**Plakous** is an embeddable single-agent, multi-provider harness. It owns the
loop, providers, profiles, auth, admission and revalidation. L0 is exactly
`plakous-model`, `plakous-tools`, `plakous-agent` and the `plakous` facade.
**plakd** is the host server and operator/batch CLI. It consumes only the facade
and higher host-layer crates, owns product policy and wire UX, and has no
second runtime. No application client or UI is shipped here. Wire DTOs belong
in `plakd-wire-types`; product handlers, settings, credential resolution and
runtime/store authority stay in plakd.

## Dev

Rust 1.100.0 is the pinned toolchain and MSRV. Supported platforms are Linux
x86_64 musl and Apple Silicon macOS. All gates require a working jj: use
`PLAK_TEST_JJ`, then `PATH`, then the newest pinned jj under `.dev-tools/jj`.

```text
just dev-tools install     # install pinned tools and the wasm32 target
just fast [--rdeps] [PKG...] # worker gate; run any affected groups too
just steps static          # workspace static group
just batch                 # Linux integration gate; set PLAK_BATCH_BASE to the pre-batch commit
just batch-mac             # macOS integration gate
just nextest ARGS          # manual tests in the isolated test environment
just steps GROUPS          # named groups; tests-install for install-family changes
just regenerate PART       # rebuild the affected derived pins or fixtures
just tighten-ceilings      # integrator: lower debt ceilings to actual counts
just --list                # other recipes
```

Production install requires exact-tree batch and install evidence and
[qualifying strict sweep evidence](docs/spec/06-build-and-support-surface.md#8-repository-settings).

Workers leave `CHANGELOG.md` and debt ceilings alone. The integrator tightens
the upper bounds with `just tighten-ceilings` before gating every landing and
cut, and commits any changes. At a cut, the integrator writes Unreleased entries
from landed commits.

For a conflict in a [derived pin or fixture](docs/spec/05-testing-and-evidence.md#derived-pins-and-fixtures),
take either side of the generated file or block, keep both sides' source
changes as appropriate, run `just regenerate <part>` for the conflicted part,
review the result, and commit it.

## Where to read

Use the map to read only the section that owns your change, not the overview
and a whole chapter. The spec owns rules, code owns values, and
`docs/user-guide/` owns usage. The [admission rule](docs/spec/00-overview.md#spec-admission)
applies when editing specs.
The tree carries no decision history; commit messages and external notes
are not prerequisites or authority for current behavior.

`.ignore` hides the hosted binding fixtures from `rg` and Plakous search; use `git grep` or `rg -uu` when changing binding identities.

| Work | Owning spec sections |
| --- | --- |
| Layering or compatibility | 00: [crate-set rule](docs/spec/00-overview.md#the-crate-set-rule) or [persisted versions](docs/spec/00-overview.md#persisted-versions) |
| Library runtime, providers, auth, tools or facade | [01](docs/spec/01-plakous-library.md): §§1–5 loop and persistence; §§6–10 providers, auth, tools and compaction; §§11–13 observation, facade and errors; §§14–15 evidence and scope |
| Product home, CLI, host, wire, prompts or accounts | [02](docs/spec/02-plak-product.md): §1 product shape; §§2–5 homes, CLI, run loop and clients; §§6–8 prompts, usage and credentials; §9 limits |
| Agent tree or workspaces | [03](docs/spec/03-orchestration.md): §§1–4 templates, workspaces and grants; §§5–13 lifecycle and tools; §§14–16 recovery and wire; §17 limits |
| Security and authority | [04](docs/spec/04-security-and-boundaries.md): §§1–2 trust, §§3–5 credentials, effects and redaction, §§6–9 remaining boundaries |
| Gates, tests, prompts, live or eval | [05](docs/spec/05-testing-and-evidence.md): §1 gates; §2 offline tests and pins; §3 evidence and live; §§4–6 reference homes, flakes and review; Harbor bakeoff for eval |
| Platforms, features, build, release or style | [06](docs/spec/06-build-and-support-surface.md): §§1–3 distribution and support; §4 features; §5 unsafe; §§6–8 build, release, dependencies and tier policy; §9 change requirements and code style |
| Configuration | [07](docs/spec/07-config-reference.md): §§1–2 discovery and grammar, §§3–4 settings, §§5–8 errors, digest, commands and presets |

## Working agreements

- Remove obsolete paths rather than preserve backward compatibility. The
  [four exceptions](docs/spec/00-overview.md#persisted-versions) are session
  export, runner job retention, the credential gateway protocol and additive
  in-place store revisions. For each of the first three, support the build's
  own and immediately previous version; keep the previous importer or speaker
  for one bump only.
- Settled invariants: a single-owner driver with one terminal result per
  admitted root turn, single-agent L0, exact-URL auth without fallback across
  routes, and single-dispatch paid compaction. Amend the owning rule if it
  changes; do not re-litigate these invariants in another change.
- Choose the simplest implementation that meets the architecture. Grow from
  a working end-to-end path; never trade it for unfinished work. Prefer
  existing libraries and avoid speculative abstractions or configuration.
- Keep visibility narrow. Parsing and presentation stay at the product
  boundary; provider names stay out of `plakous-model` and `plakous-agent`.
  Prefer explicit data flow, small modules, early returns, enums for distinct
  states, and typed errors with sources.
- Test meaningful behavior and failure modes deterministically. Use offline
  tests for daily work; live provider tests are opt-in and paid. Follow
  [test placement](docs/spec/05-testing-and-evidence.md#21-placement) and
  [comment style](docs/spec/06-build-and-support-surface.md#code-style).
- Do not install or interrupt a running session merely to review a refactor.
  For an authorized product host change, follow the tested snapshot and
  paired upgrade rules in [06 §6.3](docs/spec/06-build-and-support-surface.md#63-updating).
- Never put credentials, auth headers, private session data or unsanitized
  provider fixtures in the tree, logs or tests. No `unsafe`.
