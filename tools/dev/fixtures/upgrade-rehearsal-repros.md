# Dormant move: real-binary reproductions

These are scratch-only diagnostic cases, not permission to enable production
ownership moves. No product code is patched. Both failures precede surrender.

Build the literal bootstrap images once on Linux, from this checkout:

```sh
python3 tools/dev/upgrade_rehearsal.py build \
  --root "$PLAK_REHEARSAL_ROOT" \
  --source 9e9785efe --destination 56b9c53d8 --jobs 4
```

The root must be a dedicated owner-only directory. The recorded maintainer
images, both built with `plak/maintainer,plak/test-hooks`, are:

| Image | Revision | Binary ID |
| --- | --- | --- |
| A | `9e9785efe0c6549ee8d1c143b81db00472031dab` | `816c1472acbd` |
| B | `56b9c53d8af0f6aa3d8418aa883725aeaff43682` | `6fb516cc6cd8` |

`--destination "$PLAK_REHEARSAL_DESTINATION"` selects a later candidate. Use a
fresh scratch root (or explicitly clean the previous one) when changing either
revision; the driver refuses to silently replace an existing checkout. Each
bootstrap case checks B's read-only `version --host-upgrade-probe` response
against A's strict field/type shape before starting a host. Unknown top-level
capability fields, such as an added `live_move_protocol`, fail this check.
Actual source admission and the bootstrap remain the authoritative real-binary
compatibility proof, not the shape check alone.

The dormant cases use the real B image twice: source with the existing
test-hooks-only `PLAK_TEST_STATE_KEY=r27.e37.p37.t10`, destination at its
natural `r28.e38.p38.t11`. This simulates two keys, **not** compatibility with
the old codecs. The separate `b-*` bootstrap cases use literal A and B.
Every runtime process receives isolated HOME, PLAK_HOME, XDG directories and
TMPDIR, an empty Git configuration and only the offline loopback provider.
Empty project boundaries prevent discovery of an operator ancestor.

## Unconsumed terminal shell job

```sh
python3 tools/dev/upgrade_rehearsal.py run \
  --root "$PLAK_REHEARSAL_ROOT" --arm dormant --case repro-job
```

The host creates one root. Its first turn starts a real promoted shell
process, which waits on a fixture file; the root then answers and becomes
idle. The driver releases the process, which prints a unique output marker
and writes a completion marker. No model prompt or process-output read
consumes the terminal notice. Invoke the destination's `host upgrade` with
its exact binary ID and the dormant test capability enabled.

Observed refusal on the pinned candidate:

```text
host_upgrade_move_blocked: semantic session store refused the workflow: identity conflict
```

`repro-terminal-job.json` records the completion marker, source native heads,
refusal and receipts before cleanup. `minimized-repros.log` is the combined
run log when the two commands are redirected together.

## Finished, result-submitting real child

```sh
python3 tools/dev/upgrade_rehearsal.py run \
  --root "$PLAK_REHEARSAL_ROOT" --arm dormant --case repro-child
```

The host creates one root and spawns a real child through
`product_child_script`, not a scripted tree-child completion. An empty
`grandchild-script.json` enables the child's ordinary caller topology tools;
the child never spawns a grandchild. The root answers and idles. Release the
child's model gate; it calls `submit_result` with turn token 1 and a valid
JSON result. Wait for the root's second turn (the result wake), every child
to leave its active turn, and a graceful source stop. Restart the source,
then invoke the destination upgrade.

Observed destination preflight refusal on the pinned candidate:

```text
host_upgrade_destination_refused: plak: grant is missing
```

`repro-finished-child.json` records the root turn count, bound node locators,
statuses, native head kinds, refusal and receipts before cleanup. This differs
from the existing dormant child fixture, which interrupts a still-running
leaf and never submits a result.

Both commands exit nonzero while these failures remain; they do not turn a
refusal into a passing qualification. Each case stops only its owned scratch
processes and removes its runtime state, retaining the diagnostic JSON and
host logs. `cleanup --root "$PLAK_REHEARSAL_ROOT"` removes retained build
artifacts after investigation.

On a corrected candidate, `repro-job` additionally inspects the original
worker through the frozen retention-v1 `Lines` endpoint, twice after the
move. The same job must remain listed with exact fixture output, number 1
and next-number floor 2. These are read-only inventory calls, not native
bindings, output consumption or acknowledgements. A missing/consumed job,
changed output or regressed floor fails the case.

The corrected qualification image at
`545469fbc459aa4fe091a39386f4a2a8abeaa289` (binary `2ad6fda96f5d`, native
runner protocol 9) passes both minimal cases and the complete eight-case
dormant arm. The finished-child follow-up uses the granted topology tool
with `purpose=followup`: ordinary coordinate mail is not a new attempt.
The job source head is checked against a committed checkpoint on its
original native branch, rather than requiring a pre-drain checkpoint cursor
to stay fixed. The checks still require unchanged session/branch/line
identities, full fixture output and the unconsumed job's number floor.
Builds use separate Cargo target directories per exact revision, so
revisiting the source cannot reuse incompatible candidate artifacts.

## Bootstrap operator comparison

```sh
python3 tools/dev/upgrade_rehearsal.py run \
  --root "$PLAK_REHEARSAL_ROOT" --arm bootstrap
```

`operator-summary.md` gives the mappings and actual refusal remedies. On the
fixed images above, all four bootstrap observations pass:

- Idle roots and the parent of a finished child carry to **new successor IDs**.
  Complete canonical export history, named state and paused text queues remain
  equal after the successors actually open. Presentation-entry IDs are not
  canonical identity and are not compared across reconstructed checkpoints.
- The old source refuses execution. Its child history remains readable with A;
  B has no executable copy of that child or source tree. Keep the old snapshot
  and state for historical inspection.
- An active child refuses as `host_upgrade_blocked` / `live_child`, with the
  instruction to let it finish or close it first.
- A live process refuses as `host_upgrade_process_retention_unavailable`, with
  the instruction to settle and drain it using the old build, then retry.
- A terminal process can settle and carry if its exit notice has joined
  canonical history. Otherwise it refuses as `host_upgrade_blocked` /
  `daemon_jobs`, with the instruction to resume and prompt the old session to
  deliver its notices before retrying. Both outcomes were observed; process
  exit alone does not guarantee settled export. There is **no job-ledger
  transfer** to the new line. Read/save needed output before the bootstrap.

The capable-source `d-carry` and `d-cut-0` through `d-cut-3` cases use an
interrupted, canonically quiescent real bound leaf, matching the existing
dormant durability fixture. Their cuts are source prepare, source commit,
destination tree seed and native publication. They do not substitute for the
two failing cases above or for future live/parked-tree qualification.

The mixed dormant case stops its source host before releasing the promoted
process. The process exits under its retained original worker while no idle
driver can consume the notice during the remaining fixture inspections. Its
captured move capsule must therefore contain the full terminal output and
unchanged job-number floor, rather than a legitimately consumed exit notice
that has already joined canonical history.

The later bootstrap candidate
`d6fab92455ab7b8082b49df585ef6e06ac726c29` (binary `eabc21b8fc9c`) also
passes all four bootstrap observations from A `9e9785efe` / `816c1472acbd`.
Its destination probe passes the source's strict shape check, and the real
source completes the settled upgrade and opens successors. In this run the
terminal job's settled exit output joined history; active children and live
jobs retained the refusal remedies above. This is literal old-codec bootstrap
evidence, distinct from the simulated-key dormant qualification.
