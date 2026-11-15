# Current-format binding inspection seed

This synthetic store contains seventeen real tool presentations: native roots
and children, API roots and children across four compositions, and three
production native compositions. Construction never dispatches a model or tool.
It uses the existing release-binding fixture producer, not rewritten native rows.
The current seed includes the complete ownership-move schema, including the
execution-open guard, explicit job cut and immutable captured terminal set. It is
regenerated when that unshipped native shape changes.

The manifest pins its checksum, format identities and cases. This seed tests
current-key binding inspection and refusal before host surrender; it is not
cross-build import evidence. The predecessor fixture separately pins refusal
of a foreign native format without rewriting its bytes.

Regenerate from the source checkout on Linux, with no concurrent build or test
in that checkout:

```sh
python3 tools/dev/fixtures/current_bindings.py
just nextest run --locked -p plak --all-features --profile durability -E 'test(=host_skeleton::upgrade::host_upgrade_binding_regression_refuses_before_surrender_and_prints_live_rows)'
```

The generator temporarily enrolls `tools/dev/fixtures/previous_release.rs`, runs
its seventeen-presentation producer and restores the original module bytes in
`finally`. It makes a SQLite backup with both connections explicitly closed
before reading or hashing the database; a connection context alone commits but
does not close/checkpoint WAL. Only then does it replace the database and
checksum manifest here. No native rows are edited.

The current seed's `source_commit` pin is
`b7cbaf2b6c28bc1a1315a7bcdc10201f109578e8`; each regeneration records the current
`git rev-parse HEAD` in the manifest. Rerun after **any runtime-schema change**,
including unshipped shape changes within the same schema number, and after a
runtime codec/epoch or presentation/executor change that affects these cases.
