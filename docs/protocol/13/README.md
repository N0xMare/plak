# Client protocol 13

Use [protocol-13-wire-schema.json](../../../bin/plakd/tests/fixtures/protocol-13-wire-schema.json),
for the checked-in build-independent contract. It has both frame directions and the
shared JSON Schema draft 2020-12 definitions. Its `binary_id` is null;
`plakd wire schema` prints the serving binary's actual identity. For validation,
select `client_frame` or `host_frame` as the root and keep the document's
`$defs` in scope. The normalized pin is SHA-256 of compact JSON with all
object keys sorted, omitting only `binary_id`:
`f86d0fd6147b7d067c18a78b9174e801e7e87bc323c74b19a74960173a73030d`.

The [client frames](../../../crates/hosting/plakd-wire-types/tests/fixtures/protocol-13-client.jsonl)
and [host frames](../../../crates/hosting/plakd-wire-types/tests/fixtures/protocol-13-host.jsonl)
are NDJSON examples, checked by the wire-types tests. They illustrate
independent scenarios, not a single connection capture.

## Changes from protocol 12

The rows below follow the decoder's replaced cases, including cases that keep
their tag but no longer accept a v12 body. Here `?` means optional; null is
written explicitly. Upgrade the entire codec at once: the host serves only
protocol 13, and the v12 shapes are not a fallback.
Send `hello.proto: 13` and require `welcome.proto: 13`. The handshake shapes
are unchanged; a v12 Hello gets `frame: refused`, `kind: version_mismatch`.

| Layer and exact tag | Protocol 12 | Protocol 13 | Client action |
| --- | --- | --- | --- |
| Client `frame: request` | `{id, op}` with the v12 operation union | Same envelope with the v13 `op` union | Keep request correlation; switch the operation encoder. |
| `op: attach` | `{node}` | `{node, retained?}`, with `retained: {host_generation, revision, token}` | Offer only an exact viewport still held for this node; otherwise omit it. |
| `op: host` | `{request}` with v12 product requests | Same wrapper with the v13 product request union | Update the nested `request` and template change codec. |
| Product `request: template_put` | `{id, changes, if_match, confirmation}`, including the boolean Fast change | Same envelope; `changes` uses the v13 union | Preserve etag and confirmation; send named tier changes. |
| Template change `field: fast_mode` | `{field: "fast_mode", value: boolean}` | `{field: "service_tier", value: "fast" or "ultrafast" or null}` | Convert false to null and true to `fast`; never send `fast_mode`. |
| Host `frame: response` | `{id, outcome: {ok: Answer} or {refused: RequestRefusal}}` | Same wrapper with the v13 answer and refusal unions | Correlate by `id`; dispatch using the updated union. |
| `answer: attached` | `{attachment}` | `{attachment, transcript}`: `{mode: "first_page"}` or `{mode: "replay", from, to}` | Replace from the first page, or apply the patch chain only to the exact `from` viewport. |
| `answer: sessions` | `{rows}` with v12 directory rows | Same wrapper with the v13 rows used by `directory` | Decode both list and watch with one v13 row codec. |
| `answer: options` | `{options}` with v12 runner status, profile and template rows | Same envelope; runners, profiles, templates and workspace choices use the v13 shapes | Update both top-level and workspace-nested choices and refusals. |
| `answer: host` | `{response}` with v12 product responses | Same wrapper with the v13 product response union | Decode the nested `response` tag; don't treat product refusals as transport refusals. |
| `refusal: placement_refused` | `{runner, reason}` using the v12 placement reason | Same wrapper with the v13 reason union | Check the nested `reason` tag rather than parsing a message. |
| Placement `reason: runner_unreachable` | `{phase: string, timed_out}`, including flat `reconcile_*` strings | `{phase: {phase, subphase?}, timed_out}`; reconciliation is `phase: "reconcile"` with optional `store`, `obsolete_lines`, `terminals`, `jobs`, `stops` or `foreground` | Decode the object in options, refusals and directory status; keep the timeout bit. |
| Host `frame: event` | `{attachment, seq, class, observation, client_id?, blobs}` with v12 observations | Same envelope with the v13 observation union | Keep sequencing and blob association; replace the observation codec. |
| Host `frame: snapshot` | `{attachment, scope, schema, revision: integer, page, last, items}` | Drops `schema`; `revision` is `{host_generation, revision, token}`, and `part` replaces `page`; adds `has_older`, `older_rows`, `older_bytes`, `older_cursor?`, `live_rows` and optional item `truncated_bytes` | Assemble all parts of the newest logical page before applying it. Merge live lengths by row key and request older pages by cursor. |
| Host `frame: directory` | `{attachment, seq, snapshot, rows}` using v12 rows | Same envelope with v13 rows | Keep snapshot/merge semantics; replace rows by session ID using the new fields below. |
| Directory row `needs_operator` | Boolean | `attention`: array of typed `{reason, since, ...}` entries | Render by reason, not by a single operator flag; `interrupted_by_upgrade` is advisory. |
| Directory row `live_work.children` | Live count included parked children | Running-work count excludes parked children | Use `held_children` for those awaiting consent, not the running count. |
| Host `frame: options_progress` | `{request_id, runner, phase, row}`: nullable flat phase and v12 runner options | Same wrapper with nullable phase object and v13 runner options | Correlate as before; null still means no phase yet, not a failure. |
| Host `frame: host_lifecycle` | `{host_generation, phase, reason, target?, retry_after_ms?}` | Adds required `seconds_remaining`, optional `deadline_at`, phase `starting`, and reason `startup` | Display the host's countdown; never use it for admission or replay. |
| Observation `kind: assistant_delta` | Body was raw assistant text | Removed; admitted live text uses keyed `transcript_stream` | Append by row key and byte offset, not to a global text buffer. |
| Observation `kind: reasoning_delta` | `{location_key, text}` | Removed; admitted live reasoning uses keyed `transcript_stream` | Reconcile thinking rows by key through the baseline and patches. |
| Observation `kind: transcript_stream` | `{scope, delta}` with `assistant`, `assistant_done`, `reasoning` or `tool` | Text body is `{row, offset, end, text}` in attachment scope; tools move to `transcript_tool_stream {scope, tool}`. No streamed `assistant_done` delta. | Check UTF-8 byte offsets and seal from keyed patches or baseline; route tool previews separately. |
| Observation `kind: presentation_snapshot` | Snapshot body inside an observation | Not an observation in 13; use the `snapshot` frame | Assemble the frame parts rather than waiting for a snapshot event. |
| Observation `kind: extension` | Product observation union | Same wrapper; `profiles` and `session_profile` use v13 rows | Decode those two bodies with the new profile role shape; other tags are shared. |
| Extension `profiles` | `{rows, current, diagnostics}` with v12 roles and differences | Same wrapper with v13 roles and differences | Read named tiers and template identifiers instead of inferring either from the label. |
| Extension `session_profile` | One v12 profile row, available via profile/chrome reads | One v13 profile row, also kept in root attach state and refreshed when the switch lands | Initialize and replace from the state event; don't round trip `list_profiles` for it. |
| Profile role `fast`; difference `root_fast` | `fast: boolean`; `root_fast: {session: boolean}` | `service_tier: "fast" or "ultrafast" or null`; `root_service_tier: {session}` with the same values | Preserve the distinction between the two named tiers and Standard. |
| Observation `kind: template_readiness`; template row `tier`, `unready_reason` | Rows had a string `tier` and nullable reason text | Rows have nullable `service_tier` and tagged `readiness`: `ready` or `not_ready {reason}` for evaluated rows; `not_evaluated` for definition rows | Treat definition reads as unevaluated. Use the observation for the evaluated verdict. |
| Observation `kind: usage_limits` | Balance lines and amount entries were formatted `{label, value}` | Amount entries and balances use `{currency, used_micros?, remaining_micros?, cap_micros?, display}`; percentages unchanged | Consume micro-units; missing is not zero and `display` is not parseable data. |
| Product `response: accounts_usage` | Per-account `allowance.view` used v12 meters and balances | The allowance view uses the same typed money as `usage_limits`; recorded usage is unchanged | Share the money codec between console responses and attachment observations. |
| Product `response: refused` | For a Changes version mismatch, `workspace_read_failed` with `cause: "vcs_unavailable"` | A host-local mismatch returns `reason: "vcs_unsupported_version"` with `found` and `required`. A paired-runner mismatch still returns `workspace_read_failed` with `cause: "vcs_unavailable"` and no versions. | Show the version remedy for the typed refusal; don't infer a version from `vcs_unavailable`. |
| Product `response: profiles_list` | `{etag, current, rows, diagnostics, page}` with v12 profile summaries | Same wrapper; summaries use v13 roles and differences | Decode each summary with the new profile codec. |
| Product `response: profiles_get` | `{profile}` with a v12 summary | Same wrapper; the definition body is unchanged and the summary is v13 | Use the resolved `template` field for identity, not the display `label`. |
| Product `response: templates_list` | `{etag, rows, diagnostics, page}` with v12 template summaries and a prose `unready_reason` for this read | Same wrapper; summaries use nullable `service_tier` and `readiness: {state: "not_evaluated"}` | Don't parse a readiness sentence or infer readiness from a definition read. |
| Product `response: templates_get`; template body `extra` | `{template}` whose body carried the legacy nullable `extra` tier and declared `tools` | Body has nullable `service_tier` in place of `extra`, and keeps declared `tools` alongside `resolved_tools` | Use named tiers and use the resolved list to explain tool availability. |

Additions that don't replace a v12 tag or field:

- `op: older_history`, `answer: history_started`, `frame: history_page` and
  `refusal: cursor_expired`: prepend completed cursor pages; on expiry, attach
  for a fresh baseline.
- `op: read_transcript_row` and `answer: transcript_row`: fetch bounded body
  slices by row and UTF-8 byte offset.
- `op: attachment_status`, `answer: attachment_status_requested` and
  `frame: attachment_status`: consume the counters; only an addressed
  status frame includes `request_id`.
- `frame: blob_refused`: discard that staged blob and don't use its ID in
  an intent.
- Directory row `preview`, `cost_root`, `cost_tree` and `superseded_by`:
  display the available facts and use the successor ID instead of treating a
  predecessor as the current session.
- Profile role `template`: use the exact name; `label` remains display text.
  Template body `resolved_tools`: render the tool names and their origins
  without treating the declared `tools` list as the runtime menu.

## Reading the stream

Attach starts with the newest whole-turn page (`snapshot`); rows inside a
page are in ascending transcript order. Assemble all parts of a page before
applying it, then prepend each completed older page. Page totals and the
next older cursor are the same on every part; the cursor is present exactly
when `has_older` is true. The older-history cursor is opaque; repeat the
last completed page's cursor after a transport
loss only on the same reopened scope. A child attachment cannot read
through a previous root attachment's history binding. On `cursor_expired`,
reattach for a new first page. A page cuts between turns, including host-woken turns that
have no visible user prompt. If retention or import leaves older rows without
their turn facts, that prefix pages as one unit. If the runtime session is
gone, the saved history pages as one unit instead. A single row that cannot fit
a frame keeps a UTF-8 prefix and reports the omitted byte count. Read an
individual truncated row or repair its stream by
key and UTF-8 byte offset. For replay use the exact retained token, host
generation and revision; discard a stale replay, or replace the viewport on
`first_page`. Keep retained revisions and transcript content in memory only.
When a root repair cannot take a coherent history cut, the host ends that
attachment with `resync_required` rather than claim a bounded resident window
is complete. Discard the old token and attach for a fresh first page.

Stream offsets, baseline live lengths, omitted row bytes and older body
totals use UTF-8 bytes, not JavaScript string length or JSON wire size.
Integers such as `host_generation` and provisional row anchors use the
full unsigned 64-bit range and can exceed 2^53; a JavaScript client must
decode and re-encode them exactly (for example as `BigInt` or the original
text), never as `Number`. One frame is at most 16 MiB of JSON, so a single
snapshot part can carry a row of several MiB.
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
the other four fields are cumulative for the attachment. Unkeyed text
previews, including child and agent previews, are not sent or numbered;
their transcript still advances through patches and baselines.
`since` is host Unix seconds in UTC; `seconds_remaining` is an advisory
monotonic duration. Show `interrupted_by_upgrade` quietly. Preserve missing
money and unknown cost separately from measured zero. Do not retry a draft
automatically after a lost response; a `blob_refused` is an explicit
not-admitted outcome for the named blob, not a draft verdict.

Runner reconciliation uses `phase: reconcile` with one of `store`,
`obsolete_lines`, `terminals`, `jobs`, `stops` or `foreground` as its
`subphase`. Progress, directory status and placement refusals share the
same phase vocabulary. A broad workspace refusal preserves its timeout fact
with `phase: reconcile` and no `subphase` when no precise step is known.
Directory `live_work.children` counts pending, running
and sealing descendants, excluding parked children, and interrupted
descendants with automatic continuations. `held_children` counts only
owner-loss holds waiting for consent, not also in `children`.
`processes` counts running and stopping process views.

Console and attached allowance views use the same provider amounts. The host
converts reported cents exactly and rounds decimal credits to the nearest
micro-unit, with ties away from zero. Amounts outside the signed range are
refused, not saturated. Percentage readings are unchanged.
The OpenRouter key-limit meter cannot distinguish a null cap (no cap)
from an unreported cap, and its money entries do not carry the reset cadence
or whether BYOK spend counts against the limit. Leave those facts unknown
rather than inferring them from the amounts.

For a host-local Changes request, a known Git version mismatch returns
`vcs_unsupported_version` with `found` and `required`. A paired runner's
Git version mismatch instead returns `workspace_read_failed` with
`cause: "vcs_unavailable"` and neither version field. Runner protocol 9 has
no compatible field for the version evidence, and the host does not substitute
its own Git version.

The bounded drain countdown uses the natural-drain deadline, not the
verdict-barrier floor; an idle drain has no countdown. A paired successor
that already sent Welcome can report `starting` with reason `paired_upgrade` while
recovering its inventory. Its countdown starts in the replacement image and
does not inherit the source's drain deadline. Round remaining time up to
whole seconds, so zero means the deadline elapsed or there is no countdown.
The startup countdown is advisory: it cannot abandon a slow inventory or
change the requester's separate readiness wait. Once inventory reopens
admission and host readiness is recorded, the successor can report `running`
with reason `paired_upgrade` and no countdown. `startup` and
`cross_key_upgrade` are reserved.
A bridge that cannot reach Welcome uses the typed discovery refusal.

Codex template writes use `{"field":"service_tier","value":"fast"}` or
`"ultrafast"`; `null` removes the named tier for Standard. Template reads,
template readiness and profile roles carry the same nullable `service_tier`
instead of the legacy Fast projection. Profile differences name the exact
session tier in `root_service_tier`.

`templates_list` is a definition read: each summary has
`readiness: {"state":"not_evaluated"}`. Use the `template_readiness`
observation at session open or after RefreshTemplates for evaluated readiness:
`ready` or `not_ready` with a reason.

The root attachment snapshot includes the current `session_profile` state.
The host publishes it again after a profile switch lands, without a
`list_profiles` request.
Profile roles carry the exact template name in `template`; `label` is
for display and may omit the profile prefix. A session root with no
recorded template carries null.

`templates_get` keeps the declared extras in `tools`. The separate
`resolved_tools` list contains up to 128 model-visible names with an origin
of `core` or `template_extra`, compiled at the template's declared access
ceiling and composition. For programmatic composition this is the callable
menu inside the program. The definition read classifies native code mode
from static provider and model information, not an authenticated catalog.
It does not apply catalog-based refusals, so a menu can be present even when
the runtime would refuse the binding. MCP listings, workspace skills and
topology grants are resolved only at runtime and aren't included. A menu
that cannot be compiled from the definition is empty with a
`tool_resolution_unavailable` diagnostic.

The host serves only 13. Use the exact build's schema and pair it with
matching client artifacts before connecting. A Hello for 12 receives
`{"frame":"refused","kind":"version_mismatch","host":13,"client":12}` and no
Welcome. There is no negotiation or fallback.

The directory uses the latest retained assistant prose, or the latest
retained user prompt if there is no assistant prose. The host redacts the
preview, folds it to one plain-text line, and cuts at a UTF-8 boundary.
Costs are measured USD micro-units: a root with unproven coverage is unknown,
and an incomplete tree may report the known subtotal. Coverage depends on
matching terminal records, attribution and retained evidence, not just the
presence of a reported price.

Both list and watch rows include `superseded_by`. A predecessor with a
published same-node switch or rebind names the session its node now opens.
Other rows carry null. If the successor lookup fails, the row also carries
null: the host logs the failure once per directory worker and does not guess
a redirect.
