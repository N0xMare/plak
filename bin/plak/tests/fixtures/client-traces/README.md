# Real-client control fixtures

These fixtures come from the public terminal driving the real host, not from
hand-authored headless operations. They retain every action frame, complete
host control answers, connection boundaries and the driver's semantic barriers.
The complete raw recording also retains unsolicited observations. Runtime,
tree and dispatch oracles check those observations' effects during replay;
this is not a byte-for-byte transcript snapshot claim.

Each direction's control sequence is exact: directory, attach, new, prompt,
stop, bye, their acknowledgements and other pinned controls. Independent
opposite-direction frames may interleave differently, but each response must
follow its request and attachment-scoped controls must stay inside the granted
attachment's lifetime. Opens and semantic barriers fence both sequences; EOF
fences its own direction, allowing drained replies after a write half-close.
Missing, duplicate, extra or within-direction-reordered controls fail. Frames
are not sorted or additionally filtered. The stored duplex entries remain a
real replay stimulus, not a unique scheduling oracle.

Only three named read-only operations and their well-formed acknowledgements
may interleave: `refresh_composer_chrome` (timer-driven chrome),
`resync_transcript` (resync at a reconnect sequence boundary), and
`list_agent_acknowledgements` (an acknowledgements read). The normalizer audits
every complete pair in the retained complete raw capture; `*-reads-audit.json`
retains those pairs for separate real-bridge tests against unchanged session,
queue, transcript and tree state, after observing read completion, with no
provider dispatch. Each journey also asserts exact face/durable transcript
equality, detecting missing or duplicated content. Any other unpinned
operation fails. Adding to the set requires review, not a prefix/pattern,
scheduler, emission-control proxy or growing collection of capture variants.

Regenerate on a development test host:

```sh
just nextest run --locked --workspace --all-features --profile serial \
  --run-ignored only -E 'test(capture_real_client_host_control_traces)'
python3 tools/dev/client_traces.py --output bin/plak/tests/fixtures/client-traces \
  target/client-traces/*-raw.json
```

Use `--check` immediately after generation with the same raw capture: it checks
normalized directional controls and the capture-specific complete reads audit.
For cross-capture stability or client parity use `--check-controls`: it compares
only normalized directional controls, still validating legal-read exclusions
and causality. Audit UUIDs, legal-read pairs and SHA provenance vary by capture;
they are not a cross-round stability oracle. The private client qualifies against these public fixture IDs
at its public pin; it does not import public command or runtime handlers.

Generated UUID identity fields, numeric frame correlation IDs, host generations,
update timestamps and the generated fixture workspace identity are substituted.
Hello and welcome binary identities use independent first-seen role aliases,
`<client-binary:N>` and `<host-binary:N>`, including for `unknown`. Repeats
correlate and distinct values within a role never collapse. The reads audit's
provenance section retains raw identities, cross-role equality, original
connections and the capture's binary SHA provenance outside compared controls.
Only complete frame-free probes (one open, both EOFs, no reset or frame) are
excluded, each audited; retained connections are aliased by real connection
order. Any frame keeps a connection. Semantic
text, capabilities, activity, counts, failures and array order are retained.
The source has a fixed fixture title. Before `/new`, the driver waits for a
stable durable update and then past that update's second; before stop it waits
past the latest update second. These explicit fixture barriers avoid random
UUID tie-breaking without sorting directory results or scheduling traffic.
Capture and replay hold the image-hash fixture consistently cold. Readiness
and reconnect barriers pass only after the actual initial topology and
promptable requests are acknowledged; they observe traffic without scheduling
emissions or sorting frames.
Numeric frame aliases are per connection, preserving every request/response
correlation; excluded reads cannot consume action alias numbers.
UUID aliases are allocated in direction order, not duplex arrival order.

`real_client_control_traces_replay_root_child_switch_stop_and_reconnect` replays
the four journeys through the real `plak host connect --stdio` executable,
without a terminal or a client-owned socket resolver. `wire_host_restart` and
`wire_host_restart_switch` retain root/child twin pilots, complete indexed
per-store signatures and every reached commit cut. Recovery attaches without
replaying input, checks no handoff/continuation or extra dispatch, and uses the
recorded stop operation to settle work durably. The real-terminal crash
matrices remain separate evidence.

The face-transcript assertion compares complete typed row content, order and
multiplicity on both connections against the durable view. A live provisional
row key may become a committed anchor; that does not permit missing, duplicate
or altered content. The indexed-pilot parity test compares both root/child
wire pilots against the original terminal pilots, not merely their counts.
