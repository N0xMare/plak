# Settled bootstrap binding witness

`bin/plakd/tests/fixtures/bootstrap-v12.json` contains seventeen actual v12
exports produced with source `9e9785efe0c6549ee8d1c143b81db00472031dab`.
That source alone decodes the checked-in r27/e37 predecessor fixture, whose
seventeen synthetic heads have real native/API, root/child and composition
presentations. It creates synthetic product records and calls its normal
portable-session exporter. It does not run a model, tool, old session driver or
diagnostic against an operator's store.

Regenerate on Linux with an isolated predecessor artifact directory:

```sh
python3 tools/dev/fixtures/bootstrap_bindings.py \
  --target-dir target/bootstrap-predecessor \
  --output target/bootstrap-v12.json
```

Review the result, then replace the checked-in JSON. The template is injected
only into the pinned source's test module; it is not linked into the candidate.
The candidate's unit test imports the documents and commits fresh native heads;
the product integration imports and re-exports every document through the CLI.
The integration relocates the fixture's inert `/` placement into its scratch
workspace, and uses the existing offline model selector. All other carried
binding fields are checked unchanged. This proves the settled carrier boundary,
not exact native reopen, live-tree preservation, credentials or a real-provider
dispatch. The host cutover rehearsal remains separate.

The native fixture's README owns its regeneration. Regenerate this export
witness only when that accepted predecessor fixture or the pinned source
changes, not to make a candidate native decoder accept a foreign epoch.
