# Run plak host as a service

This setup uses a Linux systemd **user** service, never a system-wide unit.
macOS service installation is not yet supported. Use an installed build with
the `web` feature (`just install maintainer` for a source checkout), including
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

Install a tested matching snapshot, then restart the host:

```sh
just install maintainer
systemctl --user restart plak-host.service
systemctl --user status plak-host.service
journalctl --user -u plak-host.service -f
```

Use `just install release` instead for a qualified release-flavor source build.
For a UI-only deployment, replace the UI files and use
`systemctl --user reload plak-host.service`. Reload sends SIGUSR1 and keeps
sessions running; SIGTERM and SIGHUP instead perform a planned host handoff.
There is no need to reinstall an unchanged unit after updating the stable
binary link. Remote runners still need the host's matching protocol/revision/
flavor; follow the runner upgrade rules rather than substituting another build.

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
