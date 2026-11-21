# Offline host latency

Build a Linux release binary with the offline scripted model, then run the
probe with Python 3.11+ and `websockets==15.0.1` in a scratch environment:

```sh
tools/build/build-plakd.sh --release --features plakd/test-support
python3 -m venv target/host-latency-venv
target/host-latency-venv/bin/pip install 'websockets==15.0.1'
PATH="$PWD/target/host-latency-venv/bin:$PATH" \
  just perf host-latency run target/release/plakd /tmp/plak-latency-new
```

If the build uses a separate target directory, pass the binary from that
directory. The workload uses only the binary's own private runner, gateway
and host, started and stopped by the probe. It refuses an existing scratch
directory and never accepts an external host to send commands to. The
generated stores and logs stay in the scratch directory after the run.

The defaults generate 74 root records and a hot root with a multi-page
transcript and 60 retained children. After sampling the idle phase, the
fixture spawns the final, held child and verifies a durable open root
`wait_agent`. One terminal client uses `host connect --stdio`; two web clients
use real WebSocket upgrades over the private Unix listener. One web client
keeps another resident root streaming while the other alternates between the
two roots, detaching and attaching on the same connection.

The JSON lines report these measurements separately:

| Condition | Acceptance target |
| --- | --- |
| Idle with clients attached | 0 reads/s; CPU <1% of one core |
| Root waiting on child, clients attached | <5 reads/s; CPU <2% |
| Ping sent after the bulk attach request, before its last page, while the other root streams | p99 <50 ms (also reports p50) |
| Wire acceptance of a follow-up sent to the active stream, not its durable verdict | p99 <50 ms |
| Profiles list, templates list and accounts status | p95 <100 ms each; no 10-second deadline misses |
| Attach request to first baseline page for the already-resident large root | <100 ms |
| 600 seconds switching, at a two-second cadence when requests keep up | No resyncs, lag closes or replacement connections |

Latency uses a client-side monotonic clock, including decoding. Percentiles
use nearest rank over successful outcomes; the report keeps sample counts,
deadline misses, refusals and lost requests separate. A lost transport is
replaced without retrying any submitted intent. Snapshot page counts and
bytes, streamed deltas, sequence gaps, and attempted/completed switches make
underloaded or incomplete runs visible. This is an offline workload, not a
gate or a substitute for browser or gateway measurements. Only the scratch
gateway's local key-status path is involved; no upstream requests are paid.

For a quick wiring check, use e.g.
`--roots 3 --children 2 --history-turns 2 --prompt-bytes 4096 --child-bytes 256 --io-seconds 2 --switch-seconds 6`.
This profile does not produce a multi-page baseline and is not acceptance
evidence.

`just perf host-latency io PID --seconds 30` reads Linux process counters
without contacting the host, with no WebSocket dependency. `reads_per_s`
is the rate of read syscalls; `kb_per_s` includes cached data, and
`disk_kb_per_s` counts only actual storage reads. CPU uses the system clock
tick rate and can exceed 100% for a multi-threaded process.
