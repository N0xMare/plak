# Predecessor exact-reopen fixture

`manifest.json` pins the predecessor commit, database hash, runtime schema and
epoch, and seventeen synthetic bindings: six native Direct root/leaf bindings;
production API Direct, native Code Mode, JavaScript and Python root/spawning-child
bindings; and production native Code Mode, JavaScript and Python roots. The
predecessor's actual builders and compilers created and released these sessions.
The native resolution and key sources are non-dispatching test doubles, not
credentials or retained old executors. No user history or product state was
copied. Empty genesis histories isolate the binding boundary.

The Linux fixture uses `/` as an inert workspace and never invokes tools. On
Linux the candidate must reopen every exact locator with unchanged configuration
and plan identities. Native composition cases use a synthetic, empty workspace
beside the immutable dummy evaluator artifact. Direct native process bindings
come from the manifest and never execute, rather than the generating machine's
shell inode. Other platforms pass all data-only binding inspections and assert
the execution-environment refusal for Direct cases; Linux composition cases are
not opened on another platform.

The production composition builders enroll a fixed, non-executable dummy artifact
at `/tmp/plakous-binding-fixture-v1/never-invoked-worker`. Its path and bytes are
real binding inputs, not patched digests. The test creates that immutable prefix
only on Linux and refuses conflicting paths or bytes; it never invokes it or
replaces a current-build sidecar. This explicit qualification fixture prefix is
separate from per-run temporary homes. Ordinary tests never rewrite the database.

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
