# Operator deployment (private tooling)

The hidden `plakd deploy` command is for a generated Linux systemd user host
or Apple Silicon macOS LaunchAgent, with an unchanged local gateway and
optional paired runners and exact-match clients. It does not update the gateway, migrate
custody, move state across keys, or write service units. Run it from an
independent machine with Git and OpenSSH; the remote login needs a working
user service manager and an enrolled SSH host key. No Python interpreter is
needed. Name a resolvable SSH host and login; deploy uses an empty SSH config,
not per-user aliases or shared master sockets. Never run apply inside the
host or runner being replaced. For a macOS host, enroll the running
`com.plakd.host` and local gateway LaunchAgents in that user's logged-in GUI
domain. The gateway agent must run the enrolled executable directly. The
on-disk host plist and loaded arguments must match; deploy does not load or
restart a LaunchAgent.

Copy [the placeholder inventory](deploy.example.toml) to a private path
outside the checkout. Schema 2 uses `[[staging.machines]]` and optionally
`[[production.machines]]` with a `node` and its role tables. Place the gateway
with the host; omit runner and client tables where unused. Omit an unused
stage entirely. Pin the full manifest SHA-256 for every platform you enroll.
All artifacts, including those for staging, must be from one full revision
in the advertised `origin/main` first-parent history. A candidate on an
integration branch is not eligible yet. Fetch main separately if the local
checkout cannot validate its history. For a shipping release, supply the
unpacked artifact from `tools/release/package-release.sh` (`--untagged`
for a rehearsal) and pin its `manifest.json`. Linux shipping releases are
x86-64 musl.

For a maintainer host or runner, run `just build maintainer` with the full
product features and no test hooks in the exact
source checkout, then that build's
`plakd snapshot install --from <target-dir>/maintainer --source-build <target-dir> --flavor maintainer --no-link`.
Use the printed immutable directory and keep its matching installer-published
sidecars reachable at their original content-addressed paths. In the
inventory's platform table, set `flavor = "maintainer"` and pin the SHA-256
of `deploy-manifest.json`. Use the actual build triple, including
`x86_64-unknown-linux-gnu` for a GNU maintainer build. On GNU Linux both the
build-side and target installer need `/usr/bin/readelf`; the target needs
`/usr/bin/getconf` for deploy's read-only libc check. If the target's GLIBC
is too old, build a qualified snapshot on a compatible system; don't edit
the manifest or replace individual sidecars.

List every account in use with its exact name and cheap canary model. Plan
refuses any missing in-use account; signed-out accounts show `not_enrolled`.
Canaries cover Codex OAuth, Claude OAuth, SuperGrok, OpenAI API, Anthropic
API, xAI API and OpenRouter. Choose a cheap model that supports the
tool-free 16-token request at low or disabled effort. API canaries record
`provider_capped_api_v1`. The fixed request bodies require models that accept:

- OpenAI and xAI Responses: `reasoning.effort` set to `low` or `none`.
- Anthropic Messages: adaptive thinking with `output_config.effort=low`,
  or `thinking.type=disabled`.
- OpenRouter Chat: low or disabled reasoning, with parameter support
  required and provider fallback disabled.

The provider must return completed assistant text within 16 tokens.
An `incomplete` Responses result, including one that exhausts the token
budget on reasoning, is a failed canary. Choose a different model or effort;
deploy does not retry or relax the bound.

Codex has no provider-side cap: pin the model's lowest admitted effort and
review its `locally_bounded` evidence. The canary cancels at 64 observed text
or reasoning bytes or ends on completion, with a 45-second deadline before
bounded shutdown. It cannot cap provider billing.

The incumbent must provide every in-use adapter for BASELINE. Move a fleet
whose incumbent lacks an adapter to the first capable build using the
manual same-key procedure before its first deploy. The following deploy is
the first eligible automated upgrade. Updating only the destination does
not add canary support to the source; do not omit a route or sign out an
account to pass plan.

For a one-Mac fleet, use a separate macOS user for staging, not another
product home under the production UID. Give each user their own HOME,
product home, service/runner pairing, links and credentials. Linux stage
targets must be on separate machines. The operator machine is separate from
both. Use distinct evidence paths, and give both stages the same receipt path.
The SSH strings catch obvious overlap; before staging, deploy checks the
actual Linux machine digests against local production evidence and receipts,
including roles reached through a different SSH name. Deploy reads at most
32 MiB of the production evidence log for this check. If the log exceeds the
bound, rotate it or archive old, settled evidence before retrying. Do not
discard an unresolved intent to get under the limit.

A client's manifest has exactly `schema: 1`, `platform` matching its node,
`wire: {"number": N, "schema_pin": "…"}`,
and `file: {"length": N, "sha256": "…"}`. Enroll a dedicated symlink under
`~/.local/bin` and pin its existing target's digest. The new executable's
read-only `--check-connection` must actually connect and echo a single JSON
object `{"connected": true, "binary_id": "…", "wire": {"number": N,
"schema_pin": "…"}}`. Configure the client's endpoint independently before
deploy; the tool never interpolates a connection shell command or edits
client configuration.

For each paired runner, the enrolled ID, literal `login@host` SSH destination
and dedicated link must match the source host's `[runners.<id>]` table.
Do not remap that destination in the host's SSH config. The link must point
directly to an owned immutable snapshot, and must not be one of the ordinary
PATH commands that `install.sh link` moves. The runner image must report
its own `version --build-info`; deploy uses it to classify the transition.
Settle live jobs on the old build. For a job reported as `outcome_unknown`,
use the reported job number and the `jobs.ack` or `jobs.disown` operation.
If plan cannot inspect a runner, use its source-compatible command link
and attach a settled source root read-only on that runner. A picker Ready row
alone is not enough. The source reinspects the enrolled runners before drain
and before surrender, even if their build triple did not change. If it loses
inspection, it refuses the handoff and deploy restores its advanced links
when the source is proven safe. Do not advance the link manually once
deploy owns its intent.
If the incumbent predates the source-side inspection, use the manual paired
upgrade once before enrolling it.

If plan returns `bootstrap_required`, use the manual same-key procedure
for the first upgrade onto an S2 build. Follow the existing settlement and
binding checks, stage with `install.sh --no-link`, invoke the destination
snapshot's `plakd host upgrade --expect-binary ID --drain DURATION`, and
only after Ready run `install.sh link SNAPSHOT_ID`. Use this build's installer
to stage the destination: its link command cannot relink snapshots published
by pre-port builds. Do not use an ordinary
one-shot run as a bounded canary. On Linux, deploy requires the generated
`plakd-host.service` and does not adopt or rename a custom host unit. For a
fleet using a custom unit, use the manual same-key procedure or move the
settled staging service to the generated unit as a separate operator
step before enrolling it. If you have a pinned ExecStart drop-in,
first verify that the stable `~/.local/bin/plakd` link resolves to the
running snapshot; move the drop-in aside and run `systemctl --user
daemon-reload` without restarting. Plan requires the exact generated unit.
The gateway also needs the generated service with a dedicated
`~/.local/bin/plakd-gateway` command link. Migrate older units using the
gateway service procedure before enrollment. Plan checks the incumbent
holder, service and command image, and requires a gateway replacement to
support both the old and new host's negotiated protocol.

```sh
plakd deploy plan --inventory "$inventory" --stage staging
plakd deploy apply --inventory "$inventory" --stage staging
plakd deploy status --inventory "$inventory" --stage staging
```

Review the JSON plan before applying. Apply runs the old image's health and
paid canaries against the old gateway before staging. It stages immutable
bits, advances dedicated runner links, drains a changing gateway, flips its
dedicated link, and starts the inactive service. It proves the new holder
with the old host before requesting host upgrade. Then it links the Ready
host, switches exact-wire clients, and verifies. A gateway-only change leaves
the host running without requesting a handoff. The
candidate's generated unit must match the on-disk bytes before it can request
handoff; use the manual same-key procedure if the template differs.
A green staging transition writes the JSON receipt at the configured path.
An unchanged, verified re-apply is a no-op and never refreshes that receipt.
For production use the exact staging platform and client manifest digests,
equivalent runner, gateway and host transition classes and route coverage,
and an unexpired green staging receipt. Resuming a recorded production handoff
does not re-check the receipt that already qualified it. Keep the evidence and receipt in an
operator-owned directory.

On failure use the command in the JSON `retry` field. For
`needs_reconcile`, inspect the actual service and upgrade outcome before
continuing. If the replacement is already Ready, the same apply can finish
the remaining links and verification without surrender again. If the runner
cohort stopped before any host request, status or apply restores the links
once it observes the recorded source image and stable link. If the host
request might have been sent, it must also prove the exact source is idle
with no pending receipt and cancel the request before restoring. Neither
case issues a new upgrade in that invocation. If the outcome is still uncertain,
leave the runner links in place while inspecting the host. If a staging target
matches production evidence, deploy also leaves its links untouched during
withdrawal. For any runner link already advanced, verify source custody and
restore the recorded source-compatible link manually. Do not discard the
evidence log or race a manual handoff against an open intent. There is no
automatic host rollback after surrender.

If a gateway action lost its reply, status reconciles the recorded drain,
link and holder. It never repeats the drain; it can cancel a request that
never started, start from a proven clean exit, or record an already Ready
replacement. Runner links can be restored before host handoff even when
gateway health is unknown. If status cannot prove a clean exit or the
recorded replacement, inspect the generated gateway service and holder of
the credential-store lock before continuing; deploy does not switch back
an already Ready gateway. Archive older schema logs only after settling
their open intents; production promotion needs a fresh schema-5 receipt.
