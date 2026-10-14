# Monty interpreter toolchain fix

Source: crates.io `monty =1.1.0`, archive SHA-256
`c695cb21c9535024e263084451d767d965f69ef65658f24f3a7507763680b81e`.
Upstream: https://github.com/pydantic/monty/ (MIT), source revision
`591527397445bf734873252d1ee20c1b6308fe7f`.

This excluded Cargo dependency is selected by the root `[patch.crates-io]`.
The only code change is in `src/modules/json/string_cache.rs`: `drop_all`
iterates its boxed fixed-size cache with `inner.entries.iter_mut()` again.
Upstream commit `505f1044` ("ci: fix lints for rust 1.99", between v1.0.1 and
v1.1.0) changed it to `for entry in &mut inner.entries`, and
`&mut Box<[T; N]>` is not `IntoIterator` before Rust 1.99. Upstream still
declares `rust-version = "1.96"`, so the unpatched crate fails with E0277 on
the pinned Rust 1.97.1. The revert restores the v1.0.1 line; behaviour is
unchanged.
Remove the override once the workspace toolchain is 1.99 or later, or a
reviewed upstream release restores `iter_mut()`.

The manifest drops the `[[test]]` targets and dev-dependencies, whose sources
(`tests/`, `test_cases/`) are not vendored, and adds `publish = false`.
`src/modules/CLAUDE.md`, upstream contributor guidance that is not part of the
build, is omitted so it is never enrolled as instructions for this repository.
Registry bookkeeping files and the unused packaged Cargo.lock are omitted;
dependency resolution uses the root Cargo.lock. The upstream MIT license is
retained as `LICENSE` from the same source revision.
