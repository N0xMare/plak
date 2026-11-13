# Run plak host as a service

This setup uses a Linux systemd **user** service or a macOS per-user
LaunchAgent, never a system-wide service. Use the default installed
release product, which includes host/web serving, together with
its matching sidecars. The service always runs the stable `~/.local/bin/plak`
link, so keep that installation current. Install the web UI build separately;
its directory must contain `index.html` and `csp.json` as described in
[the web endpoint spec](../spec/02-plak-product.md#45-the-web-endpoint).
The installer warns, but does not refuse, if that stable link is missing or
not executable. HOME cannot contain `=` or `:`. On Linux, if you set
XDG_CONFIG_HOME, it must resolve to `$HOME/.config`, where the unit is installed.

Configure `[web] origin` to the exact HTTPS origin Tailscale Serve will publish,
and `[web] allow` for the permitted tailnet logins. Tag the serving node
and permit the owner's devices in the tailnet ACL. Serve needs access to the
Unix socket (the usual root-owned tailscaled on Linux has it). This is not
loopback dev mode: service installation never accepts `--dev-login`.

## Install

Choose explicit absolute socket and UI paths. For example:

```sh
plak host install --web "unix:$HOME/.plak/host/web/web.sock" \
  --ui "$HOME/.plak/web-ui" --dry-run
plak host install --web "unix:$HOME/.plak/host/web/web.sock" \
  --ui "$HOME/.plak/web-ui"
```

Dry run only prints the service file and creates nothing. An identical
re-install keeps the service file unchanged; any other existing file, including
an edited generated file or a symlink, is refused. To change its paths, stop the service and remove
the old file first, as shown below.

### Linux

Installation writes
`~/.config/systemd/user/plak-host.service` atomically with mode 0644 and calls
`systemctl --user daemon-reload`. It does **not** enable, start or restart
anything.

Follow the printed commands:

```sh
systemctl --user enable --now plak-host.service
# If the service must run without a login session:
loginctl enable-linger "$USER"
tailscale serve --bg "unix:$HOME/.plak/host/web/web.sock"
journalctl --user -u plak-host.service -f
```

The installer prints the exact Serve command for your chosen socket but never
runs it. Linger policy may require administrator authorization on your system;
the installer does not request it. If daemon-reload fails, the complete unit
remains in place; run `systemctl --user daemon-reload` before enabling it.

The host starts with a curated environment, not the installer shell's API keys,
proxy settings or test variables. It preserves systemd's SHELL, USER and
LOGNAME so the host can ask the login shell for the PATH its session tools
need, including language-toolchain directories. Its fallback PATH is
`$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin`. The startup journal line
reports whether the login shell answered. Configure credentials through
Plak's normal account commands. The unit pins the selected product home.
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

Installation writes `~/Library/LaunchAgents/com.plak.host.plist` atomically with
mode 0644 and prepares the private directory `~/Library/Logs/Plak` (mode 0700)
and its two private log files (mode 0600). Existing logs must be your private
regular files, not symlinks or hard links; unsafe files are refused unchanged.
It never calls `launchctl`, loads the agent or starts the host. Follow the
printed commands, which contain your actual user ID and chosen paths:

```sh
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.plak.host.plist"
# If it is not already running:
launchctl kickstart "gui/$(id -u)/com.plak.host"
tailscale serve --bg "unix:$HOME/.plak/host/web/web.sock"
tail -F "$HOME/Library/Logs/Plak/host.stdout.log" \
  "$HOME/Library/Logs/Plak/host.stderr.log"
```

`RunAtLoad` starts the agent when it is loaded, including at a later GUI login;
`KeepAlive` restarts it only after unsuccessful exit. A clean host handoff does
not trigger a restart. The job allows 90 seconds to stop and does not kill its
remaining process group on exit. It uses a private umask and runs from HOME.
This is a GUI-login agent, not a daemon that runs before login.

The agent uses the same stable binary link and explicit host arguments as the
Linux unit. A fixed shell wrapper forwards only the manager's SHELL, USER and
LOGNAME into `/usr/bin/env -i`, followed by HOME, PLAK_HOME and the same curated
PATH. Paths are separate literal arguments, not shell source. No credentials,
proxy settings or installer-shell variables are embedded or inherited by the
host. Configure credentials through Plak's normal account commands. XDG_CONFIG_HOME
does not select a LaunchAgent directory. An installed agent, even for another
product home, prevents the stdio bridge's explicit auto-start from creating an
unmanaged host; load the intended service yourself.

## Upgrade, reload and logs

Install a tested matching snapshot. For a host already supporting the upgrade
coordinator, invoke the destination build with its binary ID (shown by
`plak version`):

```sh
just install maintainer
plak version
plak host upgrade --expect-binary <id>
systemctl --user status plak-host.service
journalctl --user -u plak-host.service -f
```

On macOS, inspect the service with `launchctl print "gui/$(id -u)/com.plak.host"`
and follow the two log files shown above instead of using systemctl/journalctl.

Use `just install release` instead for a qualified release-flavor source build.
Keep the old and new snapshots, including sidecars, until the command confirms
success. Installation does not itself replace the running host.

Same-key upgrade uses planned handoff and the existing session identities.
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

Promoted jobs continue only with an unchanged exact runner revision/flavor/protocol
triple. Until cross-build job retention lands, changed-triple requests with live
foreground/background processes or unknown runner state refuse before drain;
settle and drain them using the old build, then retry. Source-owned runner
clients are checked again before surrender, without another bind or custody
transfer, including child placements. A process started during preparation can
already have been promoted by Notice when the final check refuses; it keeps
running on the source and can be stopped or drained there. Cross-key carry still
requires settled jobs and trees.
`--drain` cannot be combined with `--wait` or `--abandon`.

Cross-key upgrade supports **settled roots**, locally or on paired runners.
Let turns, children, workspace operations and daemon jobs finish; resolve held
work first. `--wait 60` closes admissions while waiting up to 60 seconds and
restores them on timeout. Unsupported destination/configuration/capacity
refusals leave the old owner authoritative. A source build without the
coordinator is refused, never silently stopped.
Only unarchived roots are checked, carried and counted against the 4096-root
limit. Archived roots stay under the old key; their work or workers are outside
this operation and are not promised to transfer.

For paired runners, install the matching snapshot and advance each runner's
dedicated command link **before** invoking host upgrade. Revision, flavor and
runner protocol must all match the destination exactly. The host checks that
command, pairing, roots, configuration and capacity through read-only runner
qualification before draining; it takes no runner custody during this check.
Normal maintainer installation must not move the dedicated link. On a mismatch,
fix the link first; keep the old build available to inspect and drain old
workers. A smaller destination or changed binding/configuration that cannot
admit the carried history refuses rather than surrendering it.

The command prints source-to-successor session IDs. Cross-key successors are
new agent lines with committed named state, conversation, generated title and
paused queued text. Explicit title overrides and queued images do not survive.
Children, candidates and live jobs are not moved; source children remain inert
references and cannot be revived in the new root. Workspace files stay in place.
The retained source export documents contain those inert facts; the successor
does not reconstruct the old topology.
No historical prompt or tool is replayed.

If interrupted, retry the same destination command and expected binary ID.
Before surrender, recovery withdraws the preparation and restores the retained
old host; retry then starts a new attempt. Plain `plak host` startup does the
same withdrawal. An Aborted attempt never sends a later destination start back
to the old key. If the retained source executable is unavailable, startup marks
the preparation Aborted and can start under its own key; restoring the source
build later completes any source archival cleanup. After surrender, the old records
cannot reopen: repair the destination if needed and recover only through its
receipts. The source exits before the destination imports under its own host
custody. Each successor is imported and freshly reconciled before its own Ready
receipt. A runner outage or changed credentials/configuration after surrender
leaves only the affected successors withheld and retried; ready local roots
still serve. Remote imports reuse the host registry's Ready connection, never a
second connection that could interrupt work on an already-Ready successor.
Retries wake on runner status changes, with exponential backoff capped at three
minutes otherwise. Fix the destination/runner and retry the same command. Imported
successors are not re-probed or re-created; post-surrender capacity changes
produce an ordinary import warning, not another strict preflight. The command
reports an unknown outcome early if no host holds the home, and extends its
wait as import/readiness makes progress. A lost success answer reports the existing successors, not another
import. Do not delete or edit `host/upgrades` to force an old owner to resume.
Client reload remains separate and never performs this operation or resends
ambiguously accepted input.

If a runner is permanently lost, invoke the serving destination executable:

```sh
plak host upgrade --abandon <successor-uuid-or-runner-id>
```

An exact successor UUID abandons one pending remote successor; a runner ID
abandons that runner's pending successors. Ready successors and local roots
cannot be abandoned. This is irreversible: the reserved successor never opens,
even if the runner returns. Repeating the command is safe. The source remains
surrendered. Once all successors are Ready or abandoned, later upgrades and
ordinary standalone session commands are no longer blocked by this operation.

The command prints the retained export paths. To recover on another placement,
copy the desired export, edit only the copy's `runner` and `workspace_root`
fields to select the destination, then run `plak session import <copy>`. For
local recovery set `runner` to `{"name":"local","runner_id":"local"}` and
`workspace_root` to an absolute local directory. Do not edit the retained
original. Normal import admission still applies; stop the host before standalone remote import, and pair/admit the new
runner normally. The import creates a new line and session ID, not the
abandoned reservation. Remove obsolete runner configuration after retiring it.

After success, receipt-aware old-key host starts and old source addresses follow
the recorded destination. Source records remain archived as well as fenced:
older builds without receipt readers honour the ordinary archived-session
refusal. Do not explicitly unarchive surrendered roots or edit their stores
using an old build; those actions can bypass that legacy protection. Keep the
source snapshots and state for reference until every successor is acknowledged.

For a UI-only deployment, replace the UI files and use
`systemctl --user reload plak-host.service`. Reload sends SIGUSR1 and keeps
sessions running; SIGTERM and SIGHUP instead perform a planned host handoff.
There is no need to reinstall an unchanged unit after updating the stable
binary link. Remote runners still need the host's matching protocol/revision/
flavor; follow the runner upgrade rules rather than substituting another build.
On macOS, the UI-only reload command is
`launchctl kill SIGUSR1 "gui/$(id -u)/com.plak.host"`.

Before an upgrade that changes the runner protocol, finish or stop all
home-local background jobs using the old build. This build uses runner
protocol 7 and does not see another protocol's home-local daemon or its jobs,
including during the embedded startup check. Settle those jobs before
installing the new build and using `/reload`; reloading does not transfer them
across protocol directories. This is separate from the coordinated host
upgrade described above, whose runner qualification checks the destination.

The Linux unit leaves WorkingDirectory unset, using systemd's user-service
default of `$HOME`; the LaunchAgent sets it to HOME explicitly. Neither chooses
a workspace or changes cwd to bypass startup checks.

## Disable or remove

There is no uninstall command. On Linux, stop the service before removing its unit:

```sh
systemctl --user disable --now plak-host.service
rm "$HOME/.config/systemd/user/plak-host.service"
systemctl --user daemon-reload
```

On macOS, unload the agent before removing its plist:

```sh
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.plak.host.plist"
rm "$HOME/Library/LaunchAgents/com.plak.host.plist"
```

This removes neither your product home nor daemon-held jobs, credentials or
web assets or log files. Manage any Tailscale Serve configuration separately.
