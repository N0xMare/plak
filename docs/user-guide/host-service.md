# Run plakd host as a service

This setup uses a Linux systemd **user** service or a macOS per-user
LaunchAgent, never a system-wide service. Use the default installed
release product, which includes host/web serving, together with
its matching sidecars. The host runs the stable `~/.local/bin/plakd` link.
The required separate [credential gateway](gateway.md) runs through
`~/.local/bin/plakd-gateway`, which normal binary installation does not move.
Install the web UI build separately;
its directory must contain `index.html` and `csp.json` as described in
[the web endpoint spec](../spec/02-plak-product.md#45-the-web-endpoint).
The installer warns, but does not refuse, if that stable link is missing or
not executable. HOME cannot contain `=` or `:`. On Linux, if you set
XDG_CONFIG_HOME, it must resolve to `$HOME/.config`, where the unit is installed.

The stdio bridge's explicit `--auto-start` refuses an installed
`plakd-host.service` or legacy `plak-host.service`, and on macOS refuses either
`com.plakd.host.plist` or legacy `com.plak.host.plist`. It reports the typed,
non-retryable `start_refused` without starting an unmanaged sibling, including
during a managed restart. This is a fail-closed presence check, not a legacy
service launcher; start the installed service explicitly.

Configure `[web] origin` to the exact HTTPS origin Tailscale Serve will publish,
and `[web] allow` for the permitted tailnet logins. Tag the serving node
and permit the owner's devices in the tailnet ACL. Serve needs access to the
Unix socket (the usual root-owned tailscaled on Linux has it). This is not
loopback dev mode: service installation never accepts `--dev-login`.

## Install

Choose explicit absolute socket and UI paths. For example:

```sh
plakd host install --web "unix:$HOME/.plak/host/web/web.sock" \
  --ui "$HOME/.plak/web-ui" --dry-run
plakd host install --web "unix:$HOME/.plak/host/web/web.sock" \
  --ui "$HOME/.plak/web-ui"
```

Dry run prints both service definitions and the default Unix
`[gateway] address`, and creates nothing. It refuses if a legacy definition
or nonempty legacy drop-in directory remains. Even after these are removed,
the rendered output does not prove that the real install will accept the
existing definitions and overrides. Inspect effective drop-ins before
migrating a service. Installation writes that address
in the product home's `config.toml` without replacing a conflicting address.
An identical re-install keeps both definitions unchanged. Edited or symlinked
units are refused. For an existing install, follow the
[one-time migration](gateway.md#migrate-an-existing-service-once).

### Linux

Installation writes
`~/.config/systemd/user/plakd-host.service` and, if absent,
`~/.config/systemd/user/plak-gateway.service` atomically with mode 0644, then calls
`systemctl --user daemon-reload`. It does **not** enable, start or restart
anything.

The host unit wants and orders after the gateway unit. Starting the host
starts the gateway first, and the host verifies the gateway is ready. A gateway
drain or crash and restart leaves a running host up.

Follow the printed commands:

```sh
systemctl --user enable --now plakd-host.service
# If the service must run without a login session:
loginctl enable-linger "$USER"
tailscale serve --bg "unix:$HOME/.plak/host/web/web.sock"
journalctl --user -u plakd-host.service -f
```

The installer prints the exact Serve command for your chosen socket but never
runs it. Linger policy may require administrator authorization on your system;
the installer does not request it. If daemon-reload fails, the complete unit
remains in place; run `systemctl --user daemon-reload` before enabling it.

The host starts with a curated environment, not the installer shell's API keys,
proxy settings or test variables. The unit sets `MALLOC_ARENA_MAX=2` to limit
glibc allocator arenas; musl ignores it. It preserves systemd's SHELL, USER and
LOGNAME so the host can ask the login shell for the PATH its session tools
need, including language-toolchain directories. Its fallback PATH is
`$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin`. The startup journal line
reports whether the login shell answered. Configure credentials through
plakd's normal account commands. The unit pins the selected product home.
It runs without SSH_AUTH_SOCK and starts in the C locale (LANG and LC_*
are not inherited). Configure remote runners with IdentityAgent or IdentityFile
in ssh config rather than relying on an interactive shell's agent socket.
systemd's runtime directory, bus address and invocation identity let the host
place its process daemon outside the host's cgroup. Restarting the host hands
sessions off and preserves promoted daemon jobs.
For installation's daemon-reload call, systemctl is resolved to an absolute
executable on your current PATH; the child gets fixed PATH `/usr/bin:/bin`
and only HOME and the user-manager runtime-directory and bus variables.

### macOS

Installation writes `~/Library/LaunchAgents/com.plakd.host.plist` and, if
absent, `com.plakd.gateway.plist` atomically with mode 0644. It prepares the
private directory `~/Library/Logs/Plak` (mode 0700)
and its two private log files (mode 0600). Existing logs must be your private
regular files, not symlinks or hard links; unsafe files are refused unchanged.
It never calls `launchctl`, loads the agent or starts the host. Follow the
printed commands, which contain your actual user ID and chosen paths:

```sh
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.plakd.gateway.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.plakd.host.plist"
# If it is not already running:
launchctl kickstart "gui/$(id -u)/com.plakd.host"
tailscale serve --bg "unix:$HOME/.plak/host/web/web.sock"
tail -F "$HOME/Library/Logs/Plak/host.stdout.log" \
  "$HOME/Library/Logs/Plak/host.stderr.log"
```

`RunAtLoad` starts the agent when it is loaded, including at a later GUI login;
`KeepAlive` restarts it only after unsuccessful exit. A clean host handoff does
not trigger a restart. The job allows 60 seconds to stop (the host's deadline is
50 seconds, with 45 seconds for session handoff) and does not kill its
remaining process group on exit. It uses a private umask and runs from HOME.
These are GUI-login agents, not daemons that run before login. launchd does
not order them; bootstrap the gateway first. At GUI login or manual startup,
the host waits for the gateway and refuses if it is not reachable.

The agent uses the same stable binary link and explicit host arguments as the
Linux unit. A fixed shell wrapper forwards only the manager's SHELL, USER and
LOGNAME into `/usr/bin/env -i`, followed by HOME, PLAK_HOME, XDG_STATE_HOME and the same curated
PATH. Paths are separate literal arguments, not shell source. No credentials,
proxy settings or installer-shell variables are embedded or inherited by the
host. Configure credentials through plakd's normal account commands. XDG_CONFIG_HOME
does not select a LaunchAgent directory. An installed agent, even for another
product home, prevents the stdio bridge's explicit auto-start from creating an
unmanaged host; load the intended service yourself.

## Upgrade, reload and logs

Use `host upgrade` only when the new build has the same state key. If a
declared format cut changes the key, follow the
[offline migration procedure](#offline-migration-at-a-format-cut) instead.

Install a tested matching snapshot. For a host already supporting the upgrade
coordinator, invoke the destination build with its binary ID (shown by
`plakd version`):

```sh
just install maintainer
plakd version
plakd host upgrade --expect-binary <id>
systemctl --user status plakd-host.service
journalctl --user -u plakd-host.service -f
```

On macOS, inspect the service with `launchctl print "gui/$(id -u)/com.plakd.host"`
and follow the two log files shown above instead of using systemctl/journalctl.

Use `just install release` instead for a qualified release-flavor source build.
Keep the old and new snapshots, including sidecars, until the command confirms
success. Installation does not itself replace the running host.

Same-key upgrade uses planned handoff and the existing session identities,
not cross-key carry. The destination checks saved child bindings before drain.
If it reports `host_upgrade_child_reopen_refused`, the current host keeps
running. Use a destination that can reopen those bindings or settle and close
the affected children before retrying.

`host_upgrade_child_reopen_pre_existing` is a separate, non-blocking line. It
appears once and gives a total child count and counts by kind:

- `evaluator_artifact_missing`: the recorded evaluator file is absent.
- `evaluator_artifact_bytes_mismatch`: the file is present but its bytes changed.
- `evaluator_artifact_unavailable`: the file cannot be read at its recorded location.
- `binding_change` or `binding_unreadable`: the source also reported a binding fault.

You can continue the upgrade. Before reopening the affected children, restore
the original evaluator file with its exact bytes and read access, or resolve
the binding fault using a compatible build. If you no longer need the children,
close them.

On Linux the upgrade replaces
the image in the running process:
the unit's `MainPID` stays the same, but the host generation changes. If you
then replace a legacy unit, stop it and start the new unit as a separate
operation; the new unit has a new PID.

On Linux, a same-key exec upgrade inherits the running environment. The
allocator setting in the new unit does not take effect until the unit is
rewritten and the service actually restarts. Until it is reinstalled,
`plakd deploy plan` refuses the old unit as template drift with the
“not the generated stable-link unit” (NORMALIZE) message. Rehearse this
separately from the cutover. After the cutover, back up and remove only the
installed `plakd-host.service` file, then run the new build's
`plakd host install` with the same `--web` and `--ui` arguments and the same
`PLAK_HOME` (the unit pins the product home).

Before the planned restart, check the effective unit:

```sh
systemctl --user show plakd-host.service -p FragmentPath,DropInPaths,NeedDaemonReload
```

It must report the generated `FragmentPath` at
`$HOME/.config/systemd/user/plakd-host.service`, empty `DropInPaths`, and
`NeedDaemonReload=no`. Then restart and verify the running process:

```sh
systemctl --user restart plakd-host.service
pid=$(systemctl --user show -p MainPID --value plakd-host.service)
tr '\0' '\n' <"/proc/$pid/environ" | grep '^MALLOC_ARENA_MAX='
```

The last command prints `MALLOC_ARENA_MAX=2`. The setting only affects glibc;
musl ignores it.

An additive product or tree store revision can advance in place without
changing the state key. The new host revises the stores after the old host
stops. Before any restart, point the service's stable executable link or
pinned `ExecStart` (on macOS, the executable in `ProgramArguments`) at the
destination.

If an inherited upgrade did not reach Ready, a cold destination start
refuses. With the destination executable and the same product home, run
`plakd host upgrade-verdict`. It prints the verdict ID and, for a known
failure, the phase. Check the destination logs and repair the cause before
clearing the fence. Stop the service manager's restart attempts and any
serving host for that home, including a predecessor started through the
unchanged service link. Do not restore a backup or rerun the handoff to clear
this failure.

Run `plakd host upgrade-verdict --acknowledge <id>` with the ID you inspected.
The command checks that the home is stopped, that the product, runtime and
tree store identities match the staged checkpoint, and that the databases
pass read-only integrity checks. A missing or nonmatching checkpoint leaves
the fence in place. Acknowledgement does not certify preservation, recognize
an older copy with the same store identity, or start the host. Point the
service at the destination and start it deliberately. If
systemd reached its restart limit, run
`systemctl --user reset-failed plakd-host.service` before starting. On macOS,
bootstrap the stopped LaunchAgent again if you unloaded it.

If startup or verdict inspection reports `host_upgrade_verdict_unreadable`,
keep the verdict and staged checkpoint. Stop restart attempts, the host, and
other writers first. The verdict is `upgrade-checkpoint-verdict` in the
product home's host directory. If only its permissions changed and you have
verified it is the original, user-owned regular file, set its mode to 0600
and retry inspection. For bad contents or a replaced file, recover only a
verified copy of the original verdict for that exact checkpoint and retry
normal acknowledgement. Never delete the unreadable verdict to bypass the
fence or invent an ID. If you cannot recover the original evidence, leave
the affected home stopped and retain it for diagnosis; use the offline
recovery procedure to recover into a separate home.

If the source build used Monty or an earlier micro-monty release for Python
cells, check Python-bound sessions before handoff. The micro-monty 0.0.3
worker cannot exactly reopen such a binding, even on a same-key upgrade.
Settle live Python roots and children on the writing build. To keep working on
an incompatible session, export it with that build and use an import format the
destination accepts. An archived head remains available for inspection, but
archiving does not make it reopenable.

The pre-upgrade copies sit beside the databases in `~/.plak/state/<key>/`,
named `product.pre-revision-<source>.sqlite3`,
`subagents.pre-revision-<source>.sqlite3`, and, for the one-time runtime
table change, `runtime.pre-codec-bootstrap.sqlite3`. They are for manual disaster
recovery, not automatic rollback. `plakd doctor` reports how many are retained
in the current state directory. To go back, stop the host and other store
users, move each revised database and its SQLite WAL/SHM files aside, restore
its pre-upgrade copy under the original name, and run the older build. This
loses writes made since the copies; it does not undo writes to other stores.
Use a coordinated backup if later work changed more than the restored stores.
Remove these copies only after verifying the new host and deciding not to
restore the earlier state.

A build supporting only tree revision 12 refuses revision 13. Do not start
it against an empty replacement tree: restore the matching pre-revision
copy under the original name, or start the newer build and fix forward.
If finished-child retirement already deleted sessions, use a coordinated
backup of all three stores, not the tree copy alone. Remove any
`[runtime_store]` retirement keys the older build does not know before
starting it; the older parser rejects new keys even when they are false.

### Back up the live stores

`plakd status --json` reports retained sessions, checkpoints, objects and accounted
object bytes against their limits, plus the feed row and byte totals.
`plakd doctor` also reports headroom. Neither the object byte limit nor the
feed totals are a disk space budget. Set `[runtime_store]` in the home config
only if you need different caps, then restart the serving host. Changing
a cap can block new work before the cap is full because settlement and
recovery have reserved headroom. If a hand edit exhausts that reserve,
doctor and host startup tell you which cap to raise.

`plakd store backup <dir>` saves the runtime, product and agent-tree SQLite
stores at a shared point in time. It does not stop the host, but can fail if
writes prevent a stable snapshot or the copy exceeds its budget. Exit 8
(`store_backup_busy`) can be retried; exit 1 for
`store_backup_destination` or `store_backup_source` needs investigation.
Choose a new directory under a private parent outside `PLAK_HOME`,
preferably on separate storage, with
room for the full databases. The command prints the same SHA-256 manifest it
saves with the files. Keep backups private: they contain session data. This
does not copy logs, job output, runner-local state, telemetry or credentials.
Each staged database must pass SQLite `quick_check` before publication.
A backup reader can hold WAL pages through shutdown: the runtime skips a
busy truncate without waiting, checkpoints what it can, and leaves the
remaining committed WAL durable. Do not remove those WAL files.

For a nightly Linux user timer, prepare a private parent, for example
`install -d -m 700 "$HOME/plak-store-backups"`. Adapt the absolute executable
and any `PLAK_HOME` setting to match the serving host. Use a
`plakd-store-backup.service` with:

```ini
[Service]
Type=oneshot
UMask=0077
ExecStart=/bin/sh -c 'exec "%h/.local/bin/plakd" store backup "%h/plak-store-backups/$$(date -u +%%Y%%m%%dT%%H%%M%%SZ)"'
```

Pair it with `plakd-store-backup.timer`:

```ini
[Timer]
OnCalendar=*-*-* 03:00:00
Persistent=true

[Install]
WantedBy=timers.target
```

Review failures in the user journal and set a retention policy for the
backup directories. For a restore, first verify all checksums, stop the host
and other store users, save the current files and their WAL/SHM files aside,
and restore all three files together under the matching state key and build.
Do not restore over a live store.

Its exact-reopen check accepts terminal job history on paired runners without
reading or watching jobs over the qualification connection or taking custody.
Live jobs across a changed runner triple still refuse with
`host_upgrade_process_retention_unavailable`: settle and drain them using the
old build, then retry.
It closes new admissions, resolves earlier draft verdicts, then drains roots
and children at runtime step boundaries. The default natural-drain budget is
60 seconds. Use `--drain 0` to skip that wait or `--drain 5m` for five minutes.
`--drain idle` waits without a natural deadline; Ctrl-C cancels preparation.
Integer `ms`, `s`, `m`, `h` and bare seconds are accepted, up to one hour.
Finite modes use one natural-drain deadline for all agents, not a separate wait
per session. The earlier verdict barrier gets at least 10 seconds, including
with `--drain 0`; that floor does not extend natural draining. Idle's verdict
barrier is bounded to 60 seconds before the unbounded boundary drain begins.

| Work still running | Finite drain at its bound | Idle |
| --- | --- | --- |
| Model call | Planned interrupt and one new continuation call | Finishes naturally under normal clocks |
| Paid compaction | Finishes within the global freeze allowance or preparation aborts; never cancelled | Finishes naturally under normal clocks |
| Child, mail or job observation wait | Notice requests a prompt result | Notice immediately |
| Foreground process tool | Notice requests promotion; the tool result must join | Notice immediately; result must join |
| Other tools, workspace/tree operations and result projection | Finish within the freeze allowance or preparation aborts | Finish naturally, with operator cancellation available |

Progress on stderr names the still-running categories, bounded driver IDs and
how long preparation has waited. A control acknowledgement is not a finished
operation. A refused promotion must finish naturally or preparation aborts.
The freeze and planned-release acknowledgements keep bounded joins in finite
modes. Idle starts surrender only after natural work reaches boundaries.
Unsuccessful preparation reopens the source's gates. Losing the requester's
connection or Ctrl-C before surrender cancels preparation; after Ready,
disconnection cannot cancel the transfer. An explicit session stop takes
precedence and is never converted into a continuation. The admission-verdict
barrier joins a delivery attempt, not receipt by a disconnected client.

Same-key upgrades can keep promoted jobs when the runner revision or flavor
changes, using retention v1. A native runner protocol change still refuses
live handoff. Source process
inspection needs a live source-compatible native connection, not just a cached
picker **Ready** row. For a cold source or a paired runner with no native backend user,
reconnect a settled root on that runner read-only using the old client and keep
it attached until the upgrade completes; do not send a prompt. Do this before
advancing the dedicated runner link. If it was already advanced, restore the old
link first, establish the old connection, then advance it again and retry.
An unavailable-inspection refusal names this remedy and leaves the source
authoritative. For the local runner, restore the old host's home-local connection
instead; there is no paired dedicated link to advance. If inspection times out
after five seconds, check runner reachability and load, wait and retry; a slow
response alone does not call for a link change or root attachment. No source restart
is needed. Same-key upgrade checks source-owned runner clients again before
surrender, without another bind or custody transfer, including child placements.
A process started during preparation can already have been promoted by Notice
when the final check refuses. It keeps running on the source and can be
stopped or drained there.

## Offline migration at a format cut

Compare the retained source and destination executables with
`plakd version --state-key`. When the keys differ, neither `host upgrade` nor
`deploy` will drain the current host. Settle the selected sessions first:
finish or stop turns and live children, resolve held work and workspace
operations, and settle jobs. Use the source build to deliver any outstanding
process results before exporting. Record the full root IDs you want to move.
For local sessions, run the source build's `plakd session list` from each
workspace. Add `--include-archived` to see archived roots, which stay behind.
For paired sessions, record the IDs from the source host's directory before
stopping it. Leave unselected sessions in the old key.

1. Keep both installed snapshots and their sidecars. If the cut requires a
   gateway protocol change, [upgrade the gateway first](gateway.md#upgrade-with-the-host).
2. Stop the source service with `systemctl --user stop plakd-host.service` on
   Linux or `launchctl bootout "gui/$(id -u)/com.plakd.host"` on macOS. If
   you started the host in a terminal, stop it there and wait for exit. Close
   standalone writers. Keep both hosts stopped until import completes.
3. With the same `PLAK_HOME`, use the retained source snapshot's executable
   to run `plakd session export <full-root-id> --out <new-private-file>` for
   each selected session. Run each local export from that session's
   workspace. For a paired root use its full ID from any directory on the
   source host machine. This build uses the saved runner workspace and does
   not connect to the runner for export. Older source builds can require the
   same absolute workspace path to exist on the source host. If that path
   is not available, use an existing source export or a source build with
   the paired export fix for that state key. Keep the original exports and
   stores.
4. Prepare the destination configuration and advance paired runners'
   dedicated links to matching snapshots. Use the destination executable to
   run `plakd session import <file>` for each export. Record the new session
   IDs it prints. If interrupted, retry only the missing imports; importing
   the same document into the same placement names its existing successor.
5. Point the service link at the destination. Start the Linux service with
   `systemctl --user start plakd-host.service` or bootstrap the macOS
   LaunchAgent as shown above. Reconnect clients using the destination's wire
   version and attach with the new session IDs. There is no redirect.

Imports preserve portable history, committed named state and paused queued
text. They do not transfer live jobs or job registries, live or held
children, tree topology, candidates or seal outcomes, effect or call
journals, workspace-operation authority, explicit title overrides, queued
images, or provider replay. Any source facts in the document are references
only. No historical tool or prompt is replayed.

An export may catch up the source projection, but it never changes source
authority. The import never writes to the source key. If you abort the
migration before starting the destination, restart the source with its own
build and matching runner links. Any imported successors are independent
sessions; they do not merge back into the source. Do not alternate between
keys after accepting new work.

For the 0.0.5 cut (`r28.e38.p38.t11` → `r28.e39.p39.t11`), rewrite named
account references in templates to provider kinds (for example,
`account = "codex/work"` becomes `account = "codex"`). Upgrade the gateway
before importing Codex or SuperGrok sessions, and use clients that match the
destination build's exact wire version.
If startup reports `host_upgrade_legacy_receipt_pending`, inspect the old
coordinated upgrade using its original builds. This host refuses pending
upgrades and acknowledged upgrades with abandoned mappings. Do not delete
the receipt.

## UI reload and runner protocols

For a UI-only deployment, replace the UI files and use
`systemctl --user reload plakd-host.service`. Reload sends SIGUSR1 and keeps
sessions running; SIGTERM and SIGHUP instead perform a planned host handoff.
There is no need to reinstall an unchanged unit after updating the stable
binary link. Remote runners still need the host's matching protocol/revision/
flavor; follow the runner upgrade rules rather than substituting another build.
On macOS, the UI-only reload command is
`launchctl kill SIGUSR1 "gui/$(id -u)/com.plakd.host"`.

Before an upgrade that changes the runner protocol, finish or stop all
home-local background jobs using the old build. This build uses runner
protocol 9 and does not see another native protocol's home-local daemon or its jobs,
including during the embedded startup check. Settle those jobs before
installing and restarting onto the new build; restart does not transfer them
across protocol directories. This is separate from the coordinated host
upgrade described above, whose runner qualification checks the destination.

Runner job retention is implemented separately from that upgrade coordinator.
The original worker/drainer keeps custody through the same-user, same-machine
`builds/<triple>/retention-v1.sock` endpoint. It offers only lines, transfer,
job snapshot/watch, read/poll, write, stop, disown and acknowledgement: it never
starts work, performs workspace effects or replays an effect. A daemon serves
its current and immediately previous retention versions; the next build speaks
both. Once a host/line becomes retention-only, reinstalling or downgrading
cannot restore native control. An incompatible endpoint requires its matching
build to settle the original jobs; if the original process is proved lost,
follow the reported lost-job remedy rather than starting or replaying it.
Retention endpoint 1, native runner protocol 9, host wire 13 and JSONL 1 are
independent versions. Retention cannot transfer jobs across a state-key cut.

The Linux unit leaves WorkingDirectory unset, using systemd's user-service
default of `$HOME`; the LaunchAgent sets it to HOME explicitly. Neither chooses
a workspace or changes cwd to bypass startup checks.

## Disable or remove

There is no uninstall command. On Linux, stop the service before removing its unit:

```sh
systemctl --user disable --now plakd-host.service
rm "$HOME/.config/systemd/user/plakd-host.service"
systemctl --user daemon-reload
```

On macOS, unload the agent before removing its plist:

```sh
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.plakd.host.plist"
rm "$HOME/Library/LaunchAgents/com.plakd.host.plist"
```

This removes neither your product home nor daemon-held jobs, credentials or
web assets or log files. Manage any Tailscale Serve configuration separately.
