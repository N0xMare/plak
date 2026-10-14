# Previous Export Fixture

`session-export-v6.json` is sanitized previous-version import evidence: the
former v4 fixture's source provenance, archive, queue, workflow ledger,
separated checkpoint encoding and typed `source_resolved_count`, written as
version 6 with its transcript as typed rows. Its tool rows carry no
`exposed_name`: that is a version 7 field, and import refuses it in a version
6 document. It is deterministic test data, not a live session or restored
runtime authority.
