# Client protocol 13 draft

Use [wire-schema.json](wire-schema.json), generated with
`plakd wire schema --protocol 13`. It has both frame directions and the
shared JSON Schema draft 2020-12 definitions. `binary_id` is null because this
is a definitions artifact, not the schema of a serving build. For validation,
select `client_frame` or `host_frame` as the root and keep the document's
`$defs` in scope. The normalized pin is SHA-256 of compact JSON with all
object keys sorted, omitting only `binary_id` (the protocol-12 algorithm).

The [client frames](../../../crates/hosting/plakd-wire-types/tests/fixtures/protocol-13-client.jsonl)
and [host frames](../../../crates/hosting/plakd-wire-types/tests/fixtures/protocol-13-host.jsonl)
are NDJSON examples, checked by the wire-types tests. They illustrate
independent scenarios, not a single connection capture.

Attach starts with the newest whole-turn page (`snapshot`); rows inside a
page are in ascending transcript order. Assemble all parts of a page before
applying it, then prepend each completed older page. Page totals and the
next older cursor are the same on every part; the cursor is present exactly
when `has_older` is true. The older-history cursor is opaque; repeat the
last completed page's cursor after a transport
loss only on the same reopened scope. On `cursor_expired`, reattach for a
new first page. Read an individual truncated row or repair its stream by
key and UTF-8 byte offset. For replay use the exact retained token, host
generation and revision; discard a stale replay, or replace the viewport on
`first_page`. Keep retained revisions and transcript content in memory only.

Stream offsets, baseline live lengths, omitted row bytes and older body
totals use UTF-8 bytes, not JavaScript string length or JSON wire size.
Assistant and reasoning deltas have the same shape; the row's type comes
from its baseline or opening patch, before the host streams that row.
The row read returns a slice of the same rendered body whose bytes were
counted; row metadata stays in the page. An attachment status report is
sent every 10 seconds while its values are changing and on request.
`seq_gaps` counts sender-visible sequence discontinuities, not losses
detected only by the client. Queue bytes and length are current gauges;
the other four fields are cumulative for the attachment.
`since` is host Unix seconds in UTC; `seconds_remaining` is an advisory
monotonic duration. Show `interrupted_by_upgrade` quietly. Preserve missing
money and unknown cost separately from measured zero. Do not retry a draft
automatically after a lost response; a `blob_refused` is an explicit
not-admitted outcome for the named blob, not a draft verdict.

This checkout still serves 12, and `plakd wire schema` without the draft
flag still reports 12. After the host implementation is complete, the
staging build will serve 13 only. Use that exact build's normal schema
output and pair it with matching client artifacts before connecting. There
is no negotiation or fallback. See the [host plan](host-plan.md) for the
activation gate.
