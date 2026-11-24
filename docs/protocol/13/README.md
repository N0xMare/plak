# Client protocol 13 draft

Use [protocol-13-wire-schema.json](../../../bin/plakd/tests/fixtures/protocol-13-wire-schema.json),
generated with `plakd wire schema --protocol 13`. It has both frame directions and the
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
new first page. A page cuts between turns, including host-woken turns that
have no visible user prompt. If retention or import leaves older rows without
their turn facts, that prefix pages as one unit. If the runtime session is
gone, the saved history pages as one unit instead. A single row that cannot fit
a frame keeps a UTF-8 prefix and reports the omitted byte count. Read an
individual truncated row or repair its stream by
key and UTF-8 byte offset. For replay use the exact retained token, host
generation and revision; discard a stale replay, or replace the viewport on
`first_page`. Keep retained revisions and transcript content in memory only.

Stream offsets, baseline live lengths, omitted row bytes and older body
totals use UTF-8 bytes, not JavaScript string length or JSON wire size.
Assistant and reasoning deltas have the same shape; the row's type comes
from its baseline or opening patch, before the host streams that row.
The root gives each contiguous assistant or reasoning run a new provisional
key. A patch settles preceding appends before opening another run; canonical
rows replace provisional keys when the host publishes the settled view.
For each live row, drop deltas wholly before the current byte length and
apply only the suffix of an overlapping delta, starting at the client's
current byte length, not the delta's start. Both offsets are UTF-8 scalar
boundaries. Repair a gap with `read_transcript_row` at the first missing
byte. That answer is addressed to the request, not a baseline or a new
transcript revision.
Its `end` is exclusive, and `last` means the slice reaches the end available
at read time, not that the row has stopped streaming. A non-boundary or
out-of-range offset is malformed; a provisional row no longer in the scope
returns `cursor_expired`. Use the settled key for omitted history after
replacement. The live assistant preview stops at a framing-safe prefix;
subsequent appends never move its start or skip to a later suffix.
The row read returns a slice of the same rendered body whose bytes were
counted; row metadata stays in the page. The resident reader caps body
slices at 64 KiB; the durable reader budgets the encoded response at
256 KiB. Request the next slice at `end` until `last` instead of inferring
the end from how many bytes arrived. An attachment status report is sent
every 10 seconds while its values are changing and on request.
`seq_gaps` counts sender-visible sequence discontinuities, not losses
detected only by the client. Queue bytes and length are current gauges;
the other four fields are cumulative for the attachment.
`since` is host Unix seconds in UTC; `seconds_remaining` is an advisory
monotonic duration. Show `interrupted_by_upgrade` quietly. Preserve missing
money and unknown cost separately from measured zero. Do not retry a draft
automatically after a lost response; a `blob_refused` is an explicit
not-admitted outcome for the named blob, not a draft verdict.

Runner reconciliation uses `phase: reconcile` with one of `store`,
`obsolete_lines`, `terminals`, `jobs`, `stops` or `foreground` as its
`subphase`. Progress, directory status and placement refusals share the
same phase vocabulary. A broad reconciliation marker does not name a
subphase on its own. Directory `live_work.children` counts pending, running
and sealing descendants, excluding parked children. `held_children` counts
descendants waiting for consent after host loss, not also in `children`.
`processes` counts running and stopping process views.

Console and attached allowance views use the same provider amounts. The host
converts reported cents exactly and rounds decimal credits to the nearest
micro-unit, with ties away from zero. Amounts outside the signed range are
refused, not saturated. Percentage readings are unchanged.
The draft OpenRouter key-limit meter cannot distinguish a null cap (no cap)
from an unreported cap, and its money entries do not carry the reset cadence
or whether BYOK spend counts against the limit. Account for that display gap
at the switch rather than inferring any of these facts from the amounts.

A known Changes version mismatch carries the observed Git version and its
minimum requirement. If the selected execution authority cannot report its
own version evidence, the host does not substitute another machine's version.

The bounded drain countdown uses the natural-drain deadline, not the
verdict-barrier floor; an idle drain has no countdown. A paired successor
that already sent Welcome can report `starting` while recovering its
startup inventory. Its countdown starts in the replacement image and does
not inherit the source's drain deadline. Round remaining time up to whole
seconds, so zero means the deadline elapsed or there is no countdown.
The startup countdown is advisory: it cannot abandon a slow inventory or
change the requester's separate readiness wait. A bridge that cannot reach
Welcome still uses the typed discovery refusal.

Codex template writes use `{"field":"service_tier","value":"fast"}` or
`"ultrafast"`; `null` removes the named tier for Standard. Template reads,
template readiness and profile roles carry the same nullable `service_tier`
instead of the legacy Fast projection. Profile differences name the exact
session tier in `root_service_tier`. Protocol 12 keeps its existing Fast toggle.

This checkout still serves 12, and `plakd wire schema` without the draft
flag still reports 12. After the host implementation is complete, the
staging build will serve 13 only. Use that exact build's normal schema
output and pair it with matching client artifacts before connecting. There
is no negotiation or fallback. See the [host plan](host-plan.md) for the
activation gate.

The draft directory uses the latest retained assistant prose, or the latest
retained user prompt if there is no assistant prose. The host redacts the
preview, folds it to one plain-text line, and cuts at a UTF-8 boundary.
Costs are measured USD micro-units: a root with unproven coverage is unknown,
and an incomplete tree may report the known subtotal. Coverage depends on
matching terminal records, attribution and retained evidence, not just the
presence of a reported price.
