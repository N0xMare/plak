# Contributing

Contributions are accepted under the repository's [MIT license](../LICENSE). By
submitting a pull request you agree to license your contribution under it.
There is no CLA and no DCO sign-off.

## Before you change anything

- Use the section map in [`AGENTS.md`](../AGENTS.md) to read the spec section
  that owns your change. Follow [spec admission](../docs/spec/00-overview.md#spec-admission)
  when updating it.
- Report security issues privately; see [SECURITY.md](SECURITY.md).

## Build and test

You need Rust 1.100.0 (pinned by `rust-toolchain.toml`), a C/C++ toolchain,
[`just`](https://github.com/casey/just), Python 3.11+ and `nm`. Supported
platforms are Apple Silicon macOS and Linux x86_64 musl.

```sh
just dev-tools install   # once: pinned cargo-nextest, cargo-deny and ruff
just fast PKG...         # worker gate; add --rdeps for dependent tests
just steps static        # workspace static group
```

There is no CI except the release workflow ([spec 06 §6.6](../docs/spec/06-build-and-support-surface.md)).
A pushed release tag builds, attests and publishes the release. A manual run
is the pre-tag dry run: the same build, packaging and attestation, with no
release. Worker branches iterate and finish with `just fast <packages>`
plus the groups their change needs; say in the pull request which gates passed
and on which platform. The integration batch, install and publication gates
are [spec 06 §8](../docs/spec/06-build-and-support-surface.md).

## Pull requests

- Keep each pull request small and focused on one change.
- Explain user-visible changes in your commits.
- Test externally meaningful behavior and failure modes deterministically.
  Live provider tests are opt-in and paid; do not add them to any gate.
- No credentials, auth headers, private session data or unsanitized provider
  captures in code, tests, fixtures or logs.
- No `unsafe`, and the narrowest visibility that works.
