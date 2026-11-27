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

An upgrade is a restart: stop the host through its service, then start the
new build on the same stores. Use it only when the new build has the same
state key. If a declared format cut changes the key, follow the
[offline migration procedure](#offline-migration-at-a-format-cut) instead.

Install a tested matching snapshot without linking it, and check it against
the running home while the old host keeps serving. These commands only
read the stores:

```sh
new=~/.local/lib/plak/maintainer/<id>/plakd
$new doctor --json
$new gateway check --json
$new store rehearse --into ~/.plak/.rehearse-<id>   # when it needs a store revision
$new session check-binding --gating [--store-probe ~/.plak/.rehearse-<id>/stores]
$new session check-reopen <session> [--store-probe ~/.plak/.rehearse-<id>/stores]
```

Compare the binding rows with the running build's own
`plakd session check-binding --all`: a row the old build verified that the
new one refuses means do not restart. `check-reopen` refuses a child the new
build cannot reopen; a missing or changed evaluator file under the same
contract is reported as pre-existing and does not refuse. Before reopening
such a child, restore the original evaluator file with its exact bytes, or
close the child if you no longer need it.

For a paired placement, `check-reopen` needs the candidate runner daemon
already serving, and it will not start one itself. Start it on the runner
machine, under the same runner account and product home, without touching
the runner's link: `<candidate-plakd> runner connect --stdio </dev/null`
starts the candidate's daemon beside the old one and returns. Then point the
probe at it with `--runner-command <runner>=<candidate-plakd>`, using the
runner's name from `[runners]` and the candidate's absolute path on the
runner machine. The old host keeps using the old daemon. Rerun a probe only
for a session that has changed since; nothing needs probing once the host
has stopped, since the stop itself records the hand-off.

Just before stopping the host, move each paired runner's link to the
candidate's snapshot. From then on the old host cannot open a new connection
to that runner, so do it last.

Then stop the host, take a backup while nothing writes the stores, move the
stable link and start the service. The backup is needed when
`store rehearse` reports a pending revision or runtime bootstrap. Without
either, the new build leaves the stores at the old build's revisions, so you
may skip it: going back means pointing the link at the old snapshot and
starting the service again. Still check that nothing writes the stores
before the start.

```sh
systemctl --user stop plakd-host.service
$new store backup ~/plak-backups/<id> --stopped
ln -sfn "$new" ~/.local/bin/plakd.next && mv -T ~/.local/bin/plakd.next ~/.local/bin/plakd
systemctl --user start plakd-host.service
journalctl --user -u plakd-host.service -f
```

`--stopped` takes both host locks and holds `run/upgrade-admission.lock`
exclusively for the backup. If it refuses, wait for the host and standalone
writers to exit and retry. Stop any other process writing directly to the
stores before taking the backup.

On macOS, stop and start with `launchctl kill SIGTERM` and `launchctl
kickstart "gui/$(id -u)/com.plakd.host"`, and follow the two log files shown
above. `launchctl kill` returns before the host exits; wait until the job
no longer has a PID in `launchctl print "gui/$(id -u)/com.plakd.host"`, then
take the stopped backup before moving the link and running `kickstart`.
A kickstart while the old job is still running does not replace it. Use
`mv -fh` instead of `mv -T` to replace the stable symlink on macOS. The stop
hands resident sessions off and ends within 50 seconds. Active root turns
and children continue once by themselves on the
new host; with the host's default `[host] resume_after_loss = "continue"`
children whose owner was lost also continue. Promoted background jobs keep running in
the runner daemon. A foreground command a turn was still waiting on ends with
the stop; the continuation turn sees that it was interrupted.

If the new build fails to start, look at its log before doing anything else.
If it never started, point the link back at the old snapshot and start the
service. Once it may have opened the stores, repair and start it again, or
start another build that supports the same store revisions. When the
upgrade applied no revision, the old build is such a build. The backup is for
disaster recovery only: stop everything using the home, restore all three
database files from one backup, and start the old build. Writes after the
backup are lost.

Keep the old and new snapshots, including sidecars, until the new host is
verified. Installation does not itself replace the running host.

A unit written by an older build takes the generated `MALLOC_ARENA_MAX=2`
setting only after you rewrite it: back up and remove only the installed
`plakd-host.service` file, then run the new build's `plakd host install` with
the same `--web` and `--ui` arguments and the same `PLAK_HOME` (the unit pins
the product home). Before the restart, check the effective unit:

```sh
systemctl --user show plakd-host.service -p FragmentPath,DropInPaths,NeedDaemonReload
```

It must report the generated `FragmentPath` at
`$HOME/.config/systemd/user/plakd-host.service`, empty `DropInPaths`, and
`NeedDaemonReload=no`. After the restart, verify the running process:

```sh
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

If the source build used Monty or an earlier micro-monty release for Python
cells, check Python-bound sessions before the restart. The micro-monty 0.0.3
worker cannot exactly reopen such a binding, even on a same-key upgrade.
Settle live Python roots and children on the writing build. To keep working on
an incompatible session, export it with that build and use an import format the
destination accepts. An archived head remains available for inspection, but
archiving does not make it reopenable.

The pre-upgrade copies sit beside the databases in `~/.plak/state/<key>/`,
named `product.pre-revision-<source>.sqlite3`,
`subagents.pre-revision-<source>.sqlite3`, `runtime.pre-codec-bootstrap.sqlite3`
for the legacy codec-table bootstrap, and `runtime.pre-admission-revision.sqlite3`
for the additive admission revision. Validation failures do not publish a
copy. A retry keeps the first copy and can refresh just one retry copy,
with suffix `-1` before `.sqlite3`. Do not open that copy as a live database.
These copies are for
manual disaster recovery, not automatic rollback. `plakd doctor` reports how many are retained
in the current state directory. To go back, stop the host and other store
users, move each revised database and its SQLite WAL/SHM files aside, restore
its pre-upgrade copy under the original name, and run the older build. This
loses writes made since the copies; it does not undo writes to other stores.
Use a coordinated backup if later work changed more than the restored stores.
Remove these copies only after verifying the new host and deciding not to
restore the earlier state.

For a rollback of only the additive runtime admission revision, use the
[lossless procedure](troubleshooting.md#store-integrity-refusal-after-a-same-key-runtime-bootstrap)
instead of restoring the copy. Do not follow an older doctor's advice to move
the store aside. First confirm the older build supports the other stores'
revisions as well.

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

## Offline migration at a format cut

Compare the retained source and destination executables with
`plakd version --state-key`. When the keys differ, a restart upgrade does
not apply. Settle the selected sessions first:
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
across protocol directories.

For a change of revision or flavor within the same runner protocol,
runner job retention keeps promoted jobs on their original daemon.
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
