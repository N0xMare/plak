# Operator deployment (private tooling)

The hidden `plakd deploy` command is for a generated Linux systemd user host
with an unchanged local gateway, a home-local runner, and any exact-match
clients enrolled on Linux x86_64. It does not update the gateway, migrate
custody, move state across keys, or render service units. Run it from an
independent machine with Git, OpenSSH, and Python 3.11+; the remote login
needs Python 3.11+, a working user service manager, and an enrolled SSH host
key. Never run apply inside the host or runner being replaced.

Copy [the placeholder inventory](deploy.example.toml) to a private path
outside the checkout. Pin full SHA-256 digests and the full revision of the
shipping artifact directory. The revision must appear on origin/main; fetch
that branch separately if the local checkout cannot validate its history.
List every canary route in use with its exact account and cheap model. Grok
and Claude OAuth models must admit the tool-free 16-token canary at the
specified low or disabled effort; do not omit a route to bypass admission.
Use `clients = []` in the stage table if there are no clients.

A client's manifest has exactly `schema: 1`, `platform:
"x86_64-unknown-linux-musl"`, `wire: {"number": N, "schema_pin": "…"}`,
and `file: {"length": N, "sha256": "…"}`. Enroll a dedicated symlink under
`~/.local/bin` and pin its existing target's digest. The new executable's
read-only `--check-connection` must actually connect and echo a single JSON
object `{"connected": true, "binary_id": "…", "wire": {"number": N,
"schema_pin": "…"}}`. Configure the client's endpoint independently before
deploy; the tool never interpolates a connection shell command or edits
client configuration.

If plan returns `bootstrap_required`, use the manual same-key procedure
for the first upgrade onto an S2 build. Follow the existing settlement and
binding checks, stage with `install.sh --no-link`, invoke the destination
snapshot's `plakd host upgrade --expect-binary ID --drain DURATION`, and
only after Ready run `install.sh link SNAPSHOT_ID`. Do not use an ordinary
one-shot run as a bounded canary. If you have a pinned ExecStart drop-in,
first verify that the stable `~/.local/bin/plakd` link resolves to the
running snapshot; move the drop-in aside and run `systemctl --user
daemon-reload` without restarting. Plan requires the exact generated unit.

```sh
plakd deploy plan --inventory "$inventory" --stage staging
plakd deploy apply --inventory "$inventory" --stage staging
plakd deploy status --inventory "$inventory" --stage staging
```

Review the JSON plan before applying. Apply runs the old image's health and
paid canaries before any target mutation, then installs, switches and verifies.
A green staging transition writes the JSON receipt at the configured path.
An unchanged, verified re-apply is a no-op and never refreshes that receipt.
For production use the exact staging manifest digests, route coverage and an
unexpired green staging receipt. Keep the evidence and receipt in an
operator-owned directory.

On failure use the command in the JSON `retry` field. For
`needs_reconcile`, inspect the actual service and upgrade outcome before
continuing. If the replacement is already Ready, the same apply can finish
the remaining links and verification without surrender again. If only the
incumbent or no owner answers, resolve that situation with the manual
same-key procedure first. Do not discard the evidence log just to retry an
uncertain surrender. There is no automatic rollback.
