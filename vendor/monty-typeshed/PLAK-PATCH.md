# Monty typeshed reproducible archive fix

Source: crates.io `monty-typeshed =1.2.0`, archive SHA-256
`7fc2079654d8d7574deb72c526e2b132c4fa7550ffc62df8b9ea1dad26d44797`.
Upstream: https://github.com/pydantic/monty/ (MIT).
License of this directory: `MIT AND Apache-2.0` (the crate is MIT; the bundled
typeshed stubs are Apache-2.0, see `TYPESHED-LICENSE`).

This excluded Cargo dependency is selected by the root `[patch.crates-io]`.
The only code changes set ZIP entry timestamps to 1980-01-01 and sort
directory entries by file name in `build.rs`, and move both `zip`
dependencies (normal and build) from 0.6.6 to 9.0.5 with default features
off (`SimpleFileOptions` replaces `FileOptions` in `build.rs`); `src/lib.rs`
gains a unit test that every archive entry carries the fixed timestamp.
zip 0.6.6 carries the extraction advisories RUSTSEC-2026-0367, -0368 and
-0369. Interpreter and Python stub contents are unchanged, and the archive
entries keep the same names, CRCs, sizes, timestamps and modes as the
zip 0.6.6 build. The crate is vendored so Cargo consumes the reviewed fix
directly, without a source-download, source-preparation, or nested Cargo
build script. Both changes are prepared as one upstream Monty pull request;
remove the override once a reviewed upstream release includes them.

The upstream MIT license is retained as `LICENSE` from source revision
`e8db9aef41aa73f4f709c15a4bc8a2d198be5ead`. Registry bookkeeping files and
the unused packaged Cargo.lock are omitted; dependency resolution uses the
root Cargo.lock.
The typeshed license is retained as `TYPESHED-LICENSE` from the exact typeshed
revision in `vendor/typeshed/source_commit.txt`. These license files are kept
outside the archived stub directory, so they do not alter generated ZIP bytes.
