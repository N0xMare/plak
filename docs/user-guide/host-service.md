# Run plak host as a service

This setup uses a Linux systemd **user** service, never a system-wide unit.
macOS service installation is not yet supported. Use the default installed
release product, which includes host/web serving, together with
its matching sidecars. The unit always runs the stable `~/.local/bin/plak`
link, so keep that installation current. Install the web UI build separately;
its directory must contain `index.html` and `csp.json` as described in
[the web endpoint spec](../spec/02-plak-product.md#45-the-web-endpoint).
The installer warns, but does not refuse, if that stable link is missing or
not executable. HOME cannot contain `=` or `:`. If you set XDG_CONFIG_HOME,
it must resolve to `$HOME/.config`, where this unit is installed.

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

Dry run only prints the unit. Installation writes
`~/.config/systemd/user/plak-host.service` atomically with mode 0644 and calls
`systemctl --user daemon-reload`. It does **not** enable, start or restart
anything. An identical re-install does nothing; any other existing file,
including an edited generated unit or a symlink, is refused. To change its
paths, disable it and remove the old file first, as shown below.

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

Use `just install release` instead for a qualified release-flavor source build.
Keep the old and new snapshots, including sidecars, until the command confirms
success. Installation does not itself replace the running host.

Same-key upgrade uses planned handoff and the existing session identities.
Cross-key upgrade supports **settled roots**, locally or on paired runners.
Let turns, children, workspace operations and daemon jobs finish; resolve held
work first. `--wait 60` closes admissions while waiting up to 60 seconds and
restores them on timeout. Unsupported destination/configuration/capacity
refusals leave the old owner authoritative. A source build without the
coordinator is refused, never silently stopped.
Archived roots are checked too: hiding a session does not settle its work.
Archived roots with live jobs or unverified settlement block upgrade; settled
archived roots remain under the old key rather than being carried.

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
still serve. Fix the destination/runner and retry the same command. Imported
successors are not re-probed or re-created; post-surrender capacity changes
produce an ordinary import warning, not another strict preflight. The command
reports an unknown outcome early if no host holds the home, and extends its
wait as import/readiness makes progress. A lost success answer reports the existing successors, not another
import. Do not delete or edit `host/upgrades` to force an old owner to resume.
Client reload remains separate and never performs this operation or resends
ambiguously accepted input.

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

Before upgrading from runner protocol 4 to 5, finish or stop all home-local
background jobs using the old build. A protocol-5 host does not see the live
protocol-4 home-local daemon or its jobs, including during the embedded
startup check. Settle those jobs before installing the new build and using
`/reload`; reloading does not transfer them across protocol directories.

The unit leaves WorkingDirectory unset, using systemd's user-service default
of `$HOME`; it does not choose a workspace or change cwd to bypass startup checks.

## Disable or remove

There is no uninstall command. Stop the service before removing its unit:

```sh
systemctl --user disable --now plak-host.service
rm "$HOME/.config/systemd/user/plak-host.service"
systemctl --user daemon-reload
```

This removes neither your product home nor daemon-held jobs, credentials or
web assets. Manage any Tailscale Serve configuration separately.
