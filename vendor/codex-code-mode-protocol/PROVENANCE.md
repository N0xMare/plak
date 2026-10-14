# Provenance: `codex-code-mode-protocol` vendor snapshot

This package is a reviewed, license-preserving copy of a small part of the
OpenAI Codex repository. It exists so that a Plakous consumer needs no
`[patch]` table to build Code Mode, and so that the model-visible `exec` and
`wait` descriptions stay byte-identical to the pinned upstream wording.

## Upstream identity

| Fact | Value |
| --- | --- |
| Repository | `https://github.com/openai/codex` |
| Revision | `b1a547b1f73ce86205d9222ac19cff334b3b7a2e` |
| Package version | `0.153.3` |
| License | Apache-2.0 (`LICENSE`, with upstream `NOTICE`) |

## Files copied

| This package | Upstream path | Lines copied | sha256 of the whole upstream file | sha256 of the vendored file |
| --- | --- | --- | --- | --- |
| `src/description.rs` | `codex-rs/code-mode-protocol/src/description.rs` | 1-503 | `a85090f7b639db03dc336937490b3230e37203da5d780d5c20b0bf378276ee24` | `05b495d8c68107ef88cc615811761372eae8d25f0fb24fb35a5fd98aa3f43be2` |
| `src/json_schema_types.rs` | `codex-rs/code-mode-protocol/src/json_schema_types.rs` | 1-534 | `266228e4e33dc9fac2c32b60e5b7e3a7365ba9e769c5a650c327f848d40306b8` | `47e37e3501ed266e8d9e6589e66c6fe366679a5c5e86ef96566f1b5e5e189ffe` |
|  `src/tool_name.rs` | `codex-rs/protocol/src/tool_name.rs` | 1-94 (whole file) | `2d102304a638001c46ffd5bd31fba05960b88a870e3db5b163b87eb9652c65ac` | `2d102304a638001c46ffd5bd31fba05960b88a870e3db5b163b87eb9652c65ac` |

The vendored digest is `sha256sum` of the file as it sits in this package. It
is what a re-vendor is checked against when the upstream checkout is not at
hand: the whole-file upstream digest cannot be reproduced from a truncated
copy, the vendored digest can. `tool_name.rs` is copied whole, so its two
digests are equal.

Each copied range is verbatim upstream text with no edit. The excluded tail of
`description.rs` and `json_schema_types.rs` is the upstream `#[cfg(test)]`
module and its `#[path]` attachment; dropping it removes a
`pretty_assertions` development dependency that a path dependency would never
build. `src/lib.rs` is written for this package: it declares the three modules,
re-exports the same public names the upstream crate exported for them, and
restates the two tool-name constants `description.rs` reads from its crate root.

Upstream `codex-code-mode-protocol` also ships gRPC, host, session, response,
and runtime modules. Plakous calls none of them, and they are what pull
`prost`, `tonic`, `tonic-prost`, `tokio-util`, `glob`, and
`protoc-bin-vendored` into a consumer's build. They are deliberately not
vendored. `tool_name.rs` replaces the former repository-only shim at
`tests/support/codex-protocol-shim` with the real upstream type.

## Updating

Re-copy the same ranges from the new revision, recompute both digest columns
with `sha256sum`, update the table above, and
re-run the Code Mode description goldens in `crates/runtimes/code-mode` and the
rendered-prompt pins in `crates/hosting/hosted/tests/common/rendered_prompts.rs`.
Any movement in those pins is a change to model-visible wording and needs its
own review.
