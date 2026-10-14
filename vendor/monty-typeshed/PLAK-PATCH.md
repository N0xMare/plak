# Monty typeshed reproducible archive fix

Source: crates.io `monty-typeshed =1.1.0`, archive SHA-256
`07a30851142ad0a76ea54df919514fc6a12571b963e1677853be22d4249a0781`.
Upstream: https://github.com/pydantic/monty/ (MIT).
License of this directory: `MIT AND Apache-2.0` (the crate is MIT; the bundled
typeshed stubs are Apache-2.0, see `TYPESHED-LICENSE`).

This excluded Cargo dependency is selected by the root `[patch.crates-io]`.
The only code changes set ZIP entry timestamps to 1980-01-01 and sort directory
entries by file name in `build.rs`. Interpreter and Python stub contents are
unchanged. The crate is vendored so Cargo consumes the reviewed fix directly,
without a source-download, source-preparation, or nested Cargo build script.
Remove the override once a reviewed upstream release includes these fixes.

The upstream MIT license is retained as `LICENSE` from source revision
`591527397445bf734873252d1ee20c1b6308fe7f`. Registry bookkeeping files and
the unused packaged Cargo.lock are omitted; dependency resolution uses the
root Cargo.lock.
The typeshed license is retained as `TYPESHED-LICENSE` from the exact typeshed
revision in `vendor/typeshed/source_commit.txt`. These license files are kept
outside the archived stub directory, so they do not alter generated ZIP bytes.
