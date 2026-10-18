# Contributing

Contributions are accepted under the repository's [MIT license](LICENSE). By
submitting a pull request you agree to license your contribution under it.
There is no CLA and no DCO sign-off.

## Before you change anything

- Read [`AGENTS.md`](AGENTS.md) and the [spec overview](docs/spec/00-overview.md),
  then the spec chapter that owns your change. `docs/spec/` is the authority for
  behavior; `docs/user-guide/` for usage.
- A change to public semantics, crate boundaries, persistence or wire formats,
  security boundaries or supported platforms updates the owning spec section in
  the same pull request.
- Report security issues privately; see [SECURITY.md](SECURITY.md).

## Build and test

You need Rust 1.99.0 (pinned by `rust-toolchain.toml`), a C/C++ toolchain,
[`just`](https://github.com/casey/just), Python 3.11+ and `nm`. Supported
platforms are Apple Silicon macOS and Linux x86_64 musl.

```sh
just dev-tools install   # once: pinned cargo-nextest and cargo-deny
just fast                # while iterating: packages you touched and their dependents
just check               # before every commit
```

There is no CI except the release workflow, which runs only when a release
tag is pushed. `just check` on your machine is the gate; say in the pull request
that it passed, and on which platform.

## Pull requests

- Keep each pull request small and focused on one change.
- Test externally meaningful behavior and failure modes deterministically.
  Live provider tests are opt-in and paid; do not add them to any gate.
- No credentials, auth headers, private session data or unsanitized provider
  captures in code, tests, fixtures or logs.
- No `unsafe`, and the narrowest visibility that works.
