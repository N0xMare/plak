# Synthetic staging A/B soak

`staging_soak.py SOURCE_RELEASE CANDIDATE_RELEASE SOURCE_SOAK CANDIDATE_SOAK
SCRATCH VERDICT --harness-revision REV --build-profile maintainer --policy POLICY`
requires Linux, Python 3.11+, `websockets==15.0.1`, GNU `readelf`, an exclusive
staging lock, a loopback-only network namespace and a private PID namespace.
Mount only the probe, its interpreter and dependencies, immutable executables
and manifests, and private writable scratch and evidence directories into the
isolated environment. Use a short, nonexistent scratch path for Unix sockets.
The probe does not accept credentials, production state, or data from another
session.

Run the nightly against the current production revision and the pinned main
tip under the exclusive staging lock. Schedule it at 03:00 UTC, outside the
reserved 05:00–05:45 and 07:00–07:45 UTC periods, with enough time for the
cycle and cleanup before either period.

Build each measured image with `tools/build/build-plakd.sh --maintainer
--features plakd/maintainer,plakd/test-support` from the clean corresponding
release revision and retain both evaluator sidecars. The recorded build
profile is supplied by the caller; the executable must report maintainer
release flavor. The probe checks each release manifest against the executable
and sidecars, and checks the actual ELF interpreter and machine against the
manifest triple. ELF ABIs without an identifiable interpreter are not
accepted. Measured images must agree with the accompanying release on source
revision, wire, state key and runner/gateway protocols. This measures
same-revision test-support variants, not the shipping executables themselves.

The probe seeds a stopped fixture with 74 roots, a heavy tree with 60 retained
children, and two large histories. Every window restores the exact same
fixture at the same private path. Windows run A/B/B/A. A serves the source;
each B starts the source from that fixture, advances the settled gateway and
local runner, then restarts the host onto the candidate (a planned stop and a
start on the same stores). B must advance the wire generation and map the
candidate image for all three roles. The probe checks the executable mappings
and digests before and after measurement.

Each window warms up for 12 seconds, then measures 48 seconds. Two parked
children remain active throughout. A fixed schedule offers two turns per
second over sixteen roots, including both large sessions. Each turn interleaves
reasoning, a real shell tool step (6 KiB stdout), and multiline assistant prose
at roughly 20 deltas per second. Sixteen fast WebSocket clients watch
continuously. Thirteen other fast clients rotate every two seconds among the
large and active roots; three slow clients rotate every four seconds and sleep
between frames. One more client attaches to a large root but stops reading;
its bounded receive queue must apply backpressure. The stalled offer is
renewed at the start of measurement. Large-session attach receipts must overlap
actual streaming on the same sessions. Busy or late slots fail rather than
shifting the schedule. Final replies drain after the fixed CPU and RSS end
sample. Warmup timings are excluded. A full A/B/B/A cycle, including fixture
setup and handoffs but not building the inputs, took about 6m20s in staging.

Each fast attachment folds its root baseline pages, keyed patches, and live
assistant appends. Unique ordered chunks let it distinguish a missing prefix
on a late attach from an interior gap. The continuously attached terminal
reference must witness the same revision and its complete assistant rows
must agree. At publication boundaries the live text must agree with the
host's keyed assistant text; a later patch that repairs missing visible prose
does not erase the mismatch. The final canonical replies are checked against
the host's durable transcript, and the permanent viewers must see every one.
Reconcile receipts record complete baselines, patches, missing delivery
sequence positions (which can also come from coalesced state), revision gaps,
missing assistant chunks, and the first mismatch for each attachment. Retained
patch replay is not counted as evicted deltas for text that was already
published before the client arrived. This is a wire/client-reconstruction
check, not a browser UI or rendering test.

Attach latency ends at the first snapshot page and is grouped by fast and
slow readers. Send-to-first-event ends at the first correlated draft event.
Send latency uses the host's monotonic enqueue time and the time the client
receives the complete WebSocket text frame, before decode. All pages of a
queued snapshot job use that job's enqueue time. The result includes probe
scheduling and receive-queue delay. The offline test-support hook adds
64 bytes of JSON whitespace to each timed frame, increasing its size and
outbox accounting slightly without changing the frame vocabulary. Historical images can time their individually traced
Pongs only; the receipt marks all other frames as unstamped. Nightly requires
complete text-frame timing on the candidate, using
`--require-candidate-send-stamps`; a partly stamped window fails.
Fast-client enqueue p50, enqueue p99 and wire ping round-trip p99 use the
named `FAST_SEND_P50_MS`, `FAST_SEND_P99_MS` and `FAST_PING_P99_MS`
bounds in `staging_soak_evidence.py`. Legacy Pong timings use their own
`LEGACY_FAST_PONG_SEND_P50_MS` and `LEGACY_FAST_PONG_SEND_P99_MS` limits,
not the frame ceilings. The verdict records all limits and a refusal
reports the observed value and bound. Slow and stalled readers are load,
not fast-client latency samples.

The CPU gate uses the sum of host, runner and gateway CPU milliseconds per
second of measured offered load. The read count is host syscalls per durable
successful tool step. RSS slope is the least-squares fit of per-second host
RSS, in MiB/min. Start and end RSS are also recorded for each serving role.
The probe records timestamps, CPU model, available logical CPUs and kernel
release. It checks quietness before each window and samples one-minute load
each second. An excursion produces an inconclusive verdict and exit 75,
with the measured load and ceiling; nightly records a skip, not a regression.
Standalone A/A controls use 0.10 per available logical CPU.

Schema 3 records the shipping and measured image pins, the measured
substitutions, the harness source revision and file hashes, host facts,
warmup and window observations, comparison deltas, attachment receipts and
the policy digest.
Compare the mean of each pair of windows for fast-client attach p95, total
CPU, host RSS slope and host end RSS. The policy supplies a relative limit
and baseline floor for each metric. With `--aa`, pass the identical images
on both sides and omit `--policy` to measure same-image variation; this uses
a same-image restart in place of the native replacement and cannot qualify a
cut. Derive the policy from at least three controls at different times, on
an otherwise quiet machine. Recalibrate when the environment or probe
changes, preserving margin above observed fast-client percentiles rather
than borrowing a slow-client bound. An independent reconciliation failure
does not erase timing evidence from a complete offered load, but the verdict
still refuses. These short windows only detect sustained growth above the
calibrated floor; slow leaks need a longer soak or production sampling.
Until three full A/A controls at different quiet times establish the nightly
bounds for the current probe, timing-only excursions remain observations.
The nightly's private policy can report the known late-attach missing
prefix and its exact patch repair on a rotating fast attachment as a
counted `known_defect`, never a pass. Other reconciliation failures and
all qualification verdicts remain strict. Remove the allowance when the
product fix lands.
A genuine CPU increase needs a recorded operator waiver if the
cut policy allows one; it does not invalidate the other checks.
`--quick` is a wiring smoke, not qualification. A same-key cut operator
can require a fresh passing verdict bound to the exact source and candidate
shipping artifacts. This is cut policy, not enforced by the product.
