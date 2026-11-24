This synthetic direct API session records the same binding used by
`current_build_api_presentation_reopens_without_dispatch`, written with
checkpoint writer epoch 38. The fixture has one checkpoint and no real
credentials, model calls, or host state.

To regenerate, run the create/release half of that test against a scratch
database in a test build with writer epoch 38 and preceding epoch 37.
Record the released locator in `locator.txt`. In the scratch database,
rebuild `runtime_objects` with its legacy `CHECK(codec=38)`, preserving
all rows, and drop `runtime_writer`. Leave the journal mode at WAL and
checkpoint the database before retaining it. No object, seal, or feed
payload is edited after creation.

The separate frozen historical store tests decoding real prior-writer
rows. This fixture holds the binding constant so codec qualification does
not also require a historical tool catalog.

SHA-256: `03f6f02f620a3374b1a96eea8d4e1bb668a60a4bed1f91dccd3c1a95f52cdc29`.
