# Predecessor exact-reopen fixture

`manifest.json` pins the predecessor commit, database hash, runtime schema and
epoch, and six synthetic native bindings (Claude, Codex and Grok; root and leaf).
The predecessor's actual configuration, registry, projection and profile compilers
created and released these sessions. No inference, credentials, user history or
product state was copied. Empty genesis histories isolate the binding boundary.

The Linux fixture uses `/` as an inert workspace and never invokes tools. On
Linux the candidate must reopen every exact locator with unchanged configuration
and plan identities. Other platforms must pass the data-only binding inspection
and then refuse the Linux execution-environment binding rather than replace it.

Regenerate for the next predecessor from a clean checkout on Linux, with an
isolated artifact directory and an output directory that does not already exist:

```sh
python3 tools/dev/generate_binding_fixture.py \
  --source <exact-predecessor-commit> \
  --target-dir target/binding-predecessor \
  --output target/previous-release-fixture
```

The recipe injects `tools/dev/fixtures/previous_release.rs` into a temporary
detached predecessor checkout, runs its ignored offline producer, and backs up
the resulting SQLite store with its manifest. It never rewrites checkpoint
identities. Review and copy both output files here, update the source-commit pin
in the consumer test, then run `previous_build_native_bindings` on the candidate.
Do not regenerate this fixture with candidate code to make a failing reopen pass.
