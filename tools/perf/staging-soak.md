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
local runner, and uses the candidate's `host upgrade --expect-binary` for the
host. B must retain the host PID, advance the wire generation, and map the
candidate image for all three roles. The probe checks the executable mappings
and digests before and after measurement.

Each window first warms up for 40 seconds with the same load, then measures
180 seconds. Two parked children remain active throughout. A fixed schedule
offers two shell tool steps per second (6 KiB of stdout each) and one attach
every two seconds per each of twelve WebSocket clients, three of which read
slowly. Busy or late slots fail rather than shifting the schedule. Every
slot gets its full period; final replies are drained after the fixed CPU and
RSS end sample. Both sides must complete every scheduled operation, with no
lag or request failures. The probe records timestamps, CPU model and load
averages, samples one-minute load each second, and refuses an excursion
above 0.10 per available logical CPU. The warmup is excluded from measured
CPU, reads, RSS and latency.

Attach latency ends at the first snapshot page and is grouped by fast and
slow readers. Send-to-first-event ends at the first correlated draft event.
The CPU gate uses the sum of host, runner and gateway CPU milliseconds per
second of measured offered load. The read count is host syscalls per durable
successful tool step. RSS slope is the least-squares fit of per-second host
RSS, in MiB/min. Start and end RSS are also recorded for each serving role.

Schema 2 records the shipping and measured image pins, the measured
substitutions, the harness source revision and file hashes, host facts,
warmup and window observations, comparison deltas, and the policy digest.
Compare the mean of each pair of windows for fast-client attach p95, total
CPU, host RSS slope and host end RSS. The policy supplies a relative limit
and baseline floor for each metric. With `--aa`, pass the identical images
on both sides and omit `--policy` to measure same-image variation; this uses
a same-image restart in place of the native replacement and cannot qualify a
cut. Derive the policy from at least three controls at different times, on
an otherwise quiet machine. Recalibrate when the environment or probe
changes. A genuine CPU increase needs a recorded operator waiver if the
cut policy allows one; it does not invalidate the other checks.
`--quick` is a wiring smoke, not qualification. A same-key cut operator
can require a fresh passing verdict bound to the exact source and candidate
shipping artifacts. This is cut policy, not enforced by the product.
