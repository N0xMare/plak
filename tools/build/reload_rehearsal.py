#!/usr/bin/env python3
"""Offline reload preflight against disposable backups, never live credentials.

A staged build whose state key differs from the session's rehearses
carry-forward: export with the installed build, then validate config, import
and rehearse with the staged build. Every home is temporary and removed.
"""

import argparse
import json
import math
from contextlib import closing
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid


ROOT = Path(__file__).resolve().parents[2]
COPY_NAMES = ("config.toml", "trusted-projects.toml", "agents", "profiles", "references")
OVERLAY_NAMES = ("config.toml", "agents", "profiles")
DATABASES = ("runtime.sqlite3", "product.sqlite3", "subagents.sqlite3")
USAGE = (
    "usage: just reload-rehearsal <full-session-uuid>"
    " [--installed <path>] [--config-overlay <dir>]"
    " [--wait-idle [--timeout S] [--interval S]] [--detach --log <path>] [--install]"
)
CARRIED_LINE = re.compile(
    r"^session ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}) linked from .+$"
)
# `plak session import` prints this one stderr line and exits 1 when
# attached_host_config cannot admit the recorded model from a Ready catalog.
# A disposable home has no credentials, so that catalog is empty. Only this
# exact line is the credential case; any other import failure still refuses.
IMPORT_CREDENTIAL_REFUSAL = "plak: session has no exact effort binding"
WAIT_IDLE_INTERVAL = 15.0
IDLE_EXPORT_REFUSALS = (
    "a turn is running or parked with an unresolved operation; let it finish or run "
    "`plak session recover` first",
    "the runtime advanced past the session record; resume the session once first",
    "background process outcomes are not settled; resume the session and send a prompt "
    "to deliver their notices before exporting",
)
ABSOLUTE_PATH = re.compile(r"/[^\s\"']+")


class UsageError(ValueError):
    pass


class SessionBusy(Exception):
    """Live-session race. The message is a closed token, never a path or SQL."""

    def __init__(self, token):
        if token not in ("database_busy", "source_changed", "carry_in_progress"):
            raise ValueError("refusal_token")
        super().__init__(token)


def _sqlite_busy(error):
    if not isinstance(error, sqlite3.Error):
        return False
    code = getattr(error, "sqlite_errorcode", None)
    if isinstance(code, int) and (code & 0xFF) in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED):
        return True
    text = str(error).lower()
    return (
        "database is locked" in text
        or "database is busy" in text
        or "database table is locked" in text
    )


def _sqlite_capture_error(error):
    if _sqlite_busy(error):
        return SessionBusy("database_busy")
    if isinstance(error, sqlite3.OperationalError):
        text = str(error).lower()
        if "unable to open database file" in text or "no such file" in text:
            return SessionBusy("source_changed")
    return error


def refusal_label(error):
    if isinstance(error, SessionBusy) or _sqlite_busy(error):
        return "session_busy_retry_when_idle"
    if isinstance(error, TimeoutError) and str(error) == "backup_deadline":
        return "session_busy_retry_when_idle"
    return "staging_or_backup_unavailable"


def refuse(error):
    # Exception text can carry private paths, SQL or config. Print the class only.
    print(f"reload rehearsal refused: {refusal_label(error)}", file=sys.stderr)
    return 1


def copy_plain(source, target):
    if source.is_symlink():
        raise ValueError("linked_copy_input")
    try:
        if source.is_dir():
            target.mkdir(mode=0o700)
            for entry in source.iterdir():
                copy_plain(entry, target / entry.name)
        elif source.is_file():
            shutil.copyfile(source, target)
            target.chmod(0o600)
        else:
            raise ValueError("unsupported_copy_input")
    except SessionBusy:
        raise
    except FileNotFoundError:
        raise SessionBusy("source_changed") from None


def backup_database(source, target):
    if source.is_symlink() or not source.is_file():
        raise ValueError("unsafe_database")
    deadline = time.monotonic() + 30

    def progress(_status, _remaining, _total):
        if time.monotonic() > deadline:
            raise TimeoutError("backup_deadline")

    # mode=ro, not immutable: a running TUI's committed WAL must be included.
    try:
        live = sqlite3.connect(source.resolve().as_uri() + "?mode=ro", uri=True, timeout=5)
    except FileNotFoundError:
        raise SessionBusy("source_changed") from None
    except sqlite3.Error as error:
        raise _sqlite_capture_error(error) from None
    copy = sqlite3.connect(target)
    try:
        live.backup(copy, pages=256, progress=progress)
        if copy.execute("PRAGMA quick_check").fetchone() != ("ok",):
            raise SessionBusy("source_changed")
    except TimeoutError as error:
        if str(error) == "backup_deadline":
            raise SessionBusy("database_busy") from None
        raise
    except sqlite3.Error as error:
        raise _sqlite_capture_error(error) from None
    finally:
        copy.close()
        live.close()
    target.chmod(0o600)


def capture_home(source, target, key):
    if not key or Path(key).name != key or key in (".", ".."):
        raise ValueError("state_key")
    target.mkdir(mode=0o700, parents=True, exist_ok=True)
    for name in COPY_NAMES:
        entry = source / name
        if entry.exists() or entry.is_symlink():
            copy_plain(entry, target / name)
    state = source / "state" / key
    if source.joinpath("state").is_symlink() or state.is_symlink():
        raise ValueError("linked_state")
    if not state.is_dir():
        raise ValueError("state_key_unavailable")
    destination = target / "state" / key
    destination.mkdir(mode=0o700, parents=True)
    for name in DATABASES:
        database = state / name
        if database.exists() or database.is_symlink():
            backup_database(database, destination / name)
    # No secrets, logs, holder locks, WAL/SHM copies, or subscription/XDG state.


def copy_named(source, target, names):
    for name in names:
        entry = source / name
        if entry.exists() or entry.is_symlink():
            copy_plain(entry, target / name)


def populate_destination(live, dest, overlay):
    """Config allowlist only. No state directory for the staged key."""
    dest.mkdir()
    dest.chmod(0o700)
    if overlay is None:
        copy_named(live, dest, COPY_NAMES)
        return None
    overlay_path = Path(overlay)
    if not overlay_path.is_absolute():
        overlay_path = Path.cwd() / overlay_path
    if overlay_path.is_symlink() or not overlay_path.is_dir():
        raise ValueError("config_overlay")
    copy_named(overlay_path, dest, OVERLAY_NAMES)
    copy_named(live, dest, tuple(name for name in COPY_NAMES if name not in OVERLAY_NAMES))
    return overlay_path


def run_environment(home, source=None):
    source = os.environ if source is None else source
    # Do not let inherited credentials, provider endpoints, telemetry exporters
    # or scripted test-hook bindings influence this read-only command.
    env = {name: source[name] for name in ("PATH", "HOME", "TMPDIR", "LANG", "LC_ALL") if name in source}
    env["PLAK_HOME"] = str(home)
    env["XDG_STATE_HOME"] = str(home / "empty-xdg-state")
    return env


def terminate(signum, _frame):
    # Unwind private copies and subprocess.run's owned-child cleanup on signals.
    raise SystemExit(128 + signum)


def session_state_key(home, session_id, staged_key):
    state = home / "state"
    if state.is_symlink():
        raise ValueError("linked_state")
    keys = []
    # Once this key owns the session, unrelated legacy schema/read failures
    # cannot make the same-key rehearsal fail.
    try:
        names = sorted(state.iterdir(), key=lambda entry: entry.name != staged_key)
    except FileNotFoundError:
        raise SessionBusy("source_changed") from None
    for entry in names:
        if entry.name != staged_key and not re.fullmatch(r"r[0-9]+\.e[0-9]+\.p[0-9]+\.t[0-9]+", entry.name):
            continue
        database = entry / "product.sqlite3"
        if entry.is_symlink() or database.is_symlink():
            raise ValueError("linked_state")
        if not database.is_file():
            continue
        try:
            with closing(sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True)) as connection:
                found = connection.execute(
                    "SELECT 1 FROM sessions WHERE id=? LIMIT 1", (session_id,)
                ).fetchone()
        except FileNotFoundError:
            raise SessionBusy("source_changed") from None
        except sqlite3.Error as error:
            raise _sqlite_capture_error(error) from None
        if found:
            if entry.name == staged_key:
                return staged_key
            keys.append(entry.name)
    if len(keys) > 1:
        raise SessionBusy("carry_in_progress")
    return keys[0] if keys else staged_key


def parse_args(argv):
    class Parser(argparse.ArgumentParser):
        def error(self, _message):
            raise UsageError(USAGE)

    parser = Parser(add_help=False)
    parser.add_argument("session_id")
    parser.add_argument("--installed")
    parser.add_argument("--config-overlay")
    parser.add_argument("--wait-idle", action="store_true")
    parser.add_argument("--timeout", type=float)
    parser.add_argument("--interval", type=float)
    parser.add_argument("--detach", action="store_true")
    parser.add_argument("--log")
    parser.add_argument("--install", action="store_true")
    options = parser.parse_args(argv)
    try:
        parsed = uuid.UUID(options.session_id)
    except ValueError:
        raise UsageError(USAGE) from None
    if str(parsed) != options.session_id:
        raise UsageError(USAGE)
    for value in (options.timeout, options.interval):
        if value is not None and (not options.wait_idle or not math.isfinite(value) or value <= 0):
            raise UsageError(USAGE)
    if options.detach and (not options.wait_idle or not options.log):
        raise UsageError(USAGE)
    if options.log and not options.detach:
        raise UsageError(USAGE)
    if options.timeout is None:
        options.timeout = 300.0
    if options.interval is None:
        options.interval = WAIT_IDLE_INTERVAL
    return options


def detach_rehearsal(options, home):
    log_path = Path(options.log).absolute()
    if _inside(log_path.resolve(), home):
        raise ValueError("log_inside_live_home")
    command = [sys.executable, str(Path(__file__).resolve()), options.session_id,
               "--wait-idle", "--timeout", str(options.timeout), "--interval", str(options.interval)]
    for flag, value in (("--installed", options.installed), ("--config-overlay", options.config_overlay)):
        if value:
            command.extend([flag, str(Path(value).absolute())])
    if options.install:
        command.append("--install")
    # O_EXCL refuses existing files/links; start_new_session performs setsid on
    # POSIX, so the worker has no caller terminal or process-group lifetime.
    descriptor = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as log:
        child = subprocess.Popen(
            command, cwd=ROOT, stdin=subprocess.DEVNULL, stdout=log,
            stderr=subprocess.STDOUT, start_new_session=True, close_fds=True,
        )
    print(f"reload rehearsal detached: pid={child.pid}", flush=True)
    return 0


def resolve_installed(explicit):
    if explicit:
        candidate = Path(explicit)
        if not candidate.is_absolute():
            candidate = Path.cwd() / candidate
    else:
        found = shutil.which("plak")
        if not found:
            return None
        candidate = Path(found)
    if not candidate.is_file():
        return None
    return candidate


def temporary_parent(live):
    # A rehearsal directory inside the live home would be a write. A live home
    # that merely sits under the system temp directory is not one.
    live = live.resolve()
    candidate = Path(tempfile.gettempdir()).resolve()
    if _inside(candidate, live):
        candidate = Path("/tmp").resolve()
        if _inside(candidate, live):
            raise ValueError("disposable_home")
    return candidate


def _inside(path, parent):
    return path == parent or parent in path.parents


def scrub(text, roots):
    if not text:
        return ""
    redacted = text
    for root in sorted({str(root) for root in roots if root}, key=len, reverse=True):
        if len(root) > 1:
            redacted = redacted.replace(root, "<path>")
    return ABSOLUTE_PATH.sub("<path>", redacted)


def emit(text, roots):
    scrubbed = scrub(text or "", roots)
    if scrubbed:
        print(scrubbed, end="" if scrubbed.endswith("\n") else "\n", flush=True)


def import_lacks_credentials(result):
    if result.returncode != 1:
        return False
    lines = [line.strip() for line in (result.stderr or "").splitlines() if line.strip()]
    return lines == [IMPORT_CREDENTIAL_REFUSAL]


def report_refusal(label, result, roots):
    text = ((result.stderr or result.stdout or "").strip()) or "failed"
    printed = False
    for line in text.splitlines():
        scrubbed = scrub(line, roots).strip()
        if not scrubbed:
            continue
        print(f"{label} refused: {scrubbed}", flush=True)
        printed = True
    if not printed:
        print(f"{label} refused: failed", flush=True)


def run_command(argv, env, cwd, timeout, on_stdout=None):
    if on_stdout is not None:
        return run_progress_command(argv, env, cwd, timeout, on_stdout)
    return subprocess.run(
        argv,
        env=env,
        cwd=cwd,
        check=False,
        capture_output=True,
        text=True,
        errors="replace",
        stdin=subprocess.DEVNULL,
        timeout=timeout,
    )


def run_progress_command(argv, env, cwd, timeout, on_stdout):
    with subprocess.Popen(
        argv, env=env, cwd=cwd, stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace",
    ) as process:
        stdout = process.stdout
        process.stdout = None  # communicate owns stderr; this joined reader owns stdout.
        lines = []

        def read_stdout():
            with stdout:
                for line in stdout:
                    lines.append(line)
                    on_stdout(line)

        reader = threading.Thread(target=read_stdout)
        reader.start()
        try:
            _, stderr = process.communicate(timeout=timeout)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()
            reader.join()
        return subprocess.CompletedProcess(argv, process.returncode, "".join(lines), stderr)


def idle_rehearsal_refusal(result):
    if result.returncode != 1 or (result.stderr or "").strip() != "plak: offline reload rehearsal refused":
        return False
    lines = (result.stdout or "").splitlines()
    busy = any(re.fullmatch(r"c[0-9]+(?:-c[0-9]+)*: busy: active_turn", line) for line in lines)
    broken = any("refuses:" in line or "=refused" in line for line in lines)
    return busy and not broken


def rehearse_copy(live, backup, key, binary, session_id, wait_idle, timeout, interval):
    deadline = time.monotonic() + timeout
    roots = [live, backup, ROOT]
    while True:
        if wait_idle and time.monotonic() >= deadline:
            print("rehearse refused: wait_idle_timeout", flush=True)
            return 1
        try:
            capture_home(live, backup, key)
            result = run_command(
                [str(binary), "session", "rehearse", session_id],
                run_environment(backup), ROOT,
                min(120, max(0.01, deadline - time.monotonic())) if wait_idle else 120,
                on_stdout=lambda line: emit(line, roots),
            )
        except SessionBusy as error:
            refuse(error)
            if not wait_idle:
                return 1
        except subprocess.TimeoutExpired:
            print("rehearse refused: inspection_timeout", flush=True)
            return 1
        else:
            if result.returncode == 0:
                return 0
            if not wait_idle or not idle_rehearsal_refusal(result):
                report_refusal("rehearse", result, roots)
                return result.returncode or 1
        print("rehearse busy: waiting for idle", flush=True)
        # Every retry must inspect a new committed snapshot of the live home.
        if backup.exists():
            shutil.rmtree(backup)
        time.sleep(min(interval, max(0, deadline - time.monotonic())))


def workspace_cwd(document, fallback):
    # The reload probe validates the session workspace, not this checkout.
    try:
        payload = json.loads(document.read_text())
    except (OSError, json.JSONDecodeError, UnicodeError):
        return fallback
    workspace = payload.get("workspace_root") if isinstance(payload, dict) else None
    if not isinstance(workspace, str) or not workspace:
        return fallback
    candidate = Path(workspace)
    if candidate.is_dir() and not candidate.is_symlink():
        return candidate
    return fallback


def carried_session(stdout):
    for line in stdout.splitlines():
        match = CARRIED_LINE.match(line.strip())
        if not match:
            continue
        try:
            canonical = str(uuid.UUID(match.group(1)))
        except ValueError:
            continue
        if canonical == match.group(1):
            return match.group(1), line.strip()
    return None


def config_problems(shown, probed):
    problems = []
    if shown.returncode != 0:
        problems.append(shown.stderr or shown.stdout or "config show failed")
    if probed.returncode != 0:
        problems.append(probed.stderr or probed.stdout or "reload probe failed")
        return problems
    try:
        payload = json.loads(probed.stdout)
    except json.JSONDecodeError:
        payload = None
    if not isinstance(payload, dict):
        problems.append("probe_unreadable")
        return problems
    error = payload.get("config_error")
    if error:
        problems.append(error if isinstance(error, str) else "probe_config_error")
    return problems


def report_config(problems, roots):
    printed = []
    for problem in problems:
        for line in str(problem).splitlines():
            scrubbed = scrub(line, roots).strip()
            if scrubbed and scrubbed not in printed:
                printed.append(scrubbed)
                print(f"config refused: {scrubbed}", flush=True)
    if not printed:
        print("config refused: invalid", flush=True)


def session_workspace(home, key, session_id):
    database = home / "state" / key / "product.sqlite3"
    with closing(sqlite3.connect(database.resolve().as_uri() + "?mode=ro", uri=True)) as connection:
        row = connection.execute(
            "SELECT workspace_root FROM sessions WHERE id=? LIMIT 1", (session_id,)
        ).fetchone()
    if not row or not isinstance(row[0], str) or not row[0]:
        raise ValueError("session_workspace_unavailable")
    workspace = Path(row[0])
    if not workspace.is_absolute() or not workspace.is_dir():
        raise ValueError("session_workspace_unavailable")
    return workspace.resolve()


def idle_export_refusal(result, session_id):
    if result.returncode != 1:
        return False
    lines = [line.strip() for line in (result.stderr or "").splitlines() if line.strip()]
    prefix = f"plak: session {session_id} cannot be exported: "
    return any(lines == [prefix + reason] for reason in IDLE_EXPORT_REFUSALS)


def export_copy(live, source_home, source_key, session_id, binary, export_path, roots,
                wait_idle, timeout, interval=WAIT_IDLE_INTERVAL):
    deadline = time.monotonic() + timeout
    while True:
        if wait_idle and time.monotonic() >= deadline:
            print("export refused: wait_idle_timeout", flush=True)
            return False
        capture_home(live, source_home, source_key)
        try:
            cwd = session_workspace(source_home, source_key, session_id)
        except ValueError as error:
            if str(error) != "session_workspace_unavailable":
                raise
            print("export refused: session_workspace_unavailable", flush=True)
            return False
        exported = run_command(
            [str(binary), "session", "export", session_id, "--out", str(export_path)],
            run_environment(source_home), cwd,
            min(120, max(0.01, deadline - time.monotonic())) if wait_idle else 120,
        )
        if exported.returncode == 0 and export_path.is_file():
            return True
        if not wait_idle or not idle_export_refusal(exported, session_id):
            report_refusal("export", exported, [*roots, cwd])
            return False
        print("export busy: waiting for idle", flush=True)
        # A retry must observe fresh committed WAL state, not the same frozen
        # snapshot that just refused. Only owned disposable paths are removed.
        shutil.rmtree(source_home)
        export_path.unlink(missing_ok=True)
        time.sleep(min(interval, max(0, deadline - time.monotonic())))


def rehearse_carry(live, temporary, staged, session_id, source_key, staged_key, installed, overlay,
                   wait_idle=False, timeout=300, interval=WAIT_IDLE_INTERVAL):
    binary = resolve_installed(installed)
    if binary is None:
        print(
            f"state key changed {source_key} -> {staged_key}: "
            "carry-forward refused: installed_build_unavailable",
            flush=True,
        )
        return 1
    print(f"state key changed {source_key} -> {staged_key}: rehearsing carry-forward", flush=True)
    source_home = temporary / "source-home"
    export_path = temporary / "session-export.json"
    roots = [live, temporary, source_home, export_path, binary]
    if not export_copy(live, source_home, source_key, session_id, binary, export_path, roots,
                       wait_idle, timeout, interval):
        return 1
    print("export=ok", flush=True)
    destination = temporary / "destination"
    try:
        overlay_path = populate_destination(live, destination, overlay)
    except ValueError as error:
        if str(error) != "config_overlay":
            raise
        print("carry-forward refused: config_overlay_unavailable", flush=True)
        return 1
    roots.append(destination)
    if overlay_path is not None:
        roots.append(overlay_path)
    cwd = workspace_cwd(export_path, destination)
    roots.append(cwd)
    env = run_environment(destination)
    shown = run_command([str(staged), "config", "show"], env, cwd, 30)
    probed = run_command([str(staged), "version", "--reload-probe"], env, cwd, 30)
    problems = config_problems(shown, probed)
    if problems:
        report_config(problems, roots)
        return 1
    print("config=valid", flush=True)
    imported = run_command(
        [str(staged), "session", "import", str(export_path)],
        env,
        cwd,
        120,
    )
    if imported.returncode != 0:
        if import_lacks_credentials(imported):
            print(
                "import=not rehearsed: credentials unavailable in the disposable home "
                "(export, config and document conversion rehearsed)",
                flush=True,
            )
            return 0
        report_refusal("import", imported, roots)
        return 1
    carried = carried_session(imported.stdout)
    if carried is None:
        print("import refused: carried_session_unreported", flush=True)
        emit(imported.stdout, roots)
        emit(imported.stderr, roots)
        return 1
    new_id, line = carried
    print(f"carried_session={new_id}", flush=True)
    emit(line, roots)
    if imported.stderr.strip():
        emit(imported.stderr, roots)
    rehearsed = run_command([str(staged), "session", "rehearse", new_id], env, cwd, 120)
    emit(rehearsed.stdout, roots)
    if rehearsed.returncode != 0:
        if rehearsed.stderr.strip():
            report_refusal("rehearse", rehearsed, roots)
        return rehearsed.returncode or 1
    return 0


def main(argv):
    try:
        options = parse_args(argv)
    except UsageError as error:
        print(error, file=sys.stderr)
        return 1
    home = Path(os.environ.get("PLAK_HOME", str(Path.home() / ".plak")))
    if not home.is_absolute():
        raise ValueError("absolute_home_required")
    home = home.resolve(strict=True)
    if options.detach:
        return detach_rehearsal(options, home)
    status = rehearse(home, options)
    if status == 0 and options.install:
        return subprocess.run(["just", "build-maintainer"], cwd=ROOT, check=False).returncode
    return status


def rehearse(home, options):
    session_id = options.session_id
    for signum in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, terminate)
    # Match build-plak.sh: independent resolutions isolate the Monty runtime.
    subprocess.run(
        ["cargo", "build", "--locked", "--offline", "-p", "plak", "--features", "plak/maintainer", "--profile", "maintainer"],
        cwd=ROOT,
        check=True,
    )
    subprocess.run(
        ["cargo", "build", "--locked", "--offline", "-p", "plakous-js-runtime",
         "-p", "plakous-py-worker", "--profile", "maintainer"],
        cwd=ROOT, check=True,
    )
    target = Path(os.environ.get("CARGO_TARGET_DIR", str(ROOT / "target")))
    if not target.is_absolute():
        target = ROOT / target
    output = target / os.environ.get("CARGO_BUILD_TARGET", "") / "maintainer"
    subprocess.run(
        [sys.executable, str(ROOT / "tools/build/check-artifact-linkage.py"), str(output)],
        cwd=ROOT, check=True,
    )
    with tempfile.TemporaryDirectory(prefix="plak-reload-rehearsal-", dir=temporary_parent(home)) as directory:
        temporary = Path(directory).resolve()
        if temporary == home or home in temporary.parents:
            raise ValueError("disposable_home")
        stage = temporary / "stage"
        stage.mkdir(mode=0o700)
        for name in ("plak", "plakous-js-host", "monty"):
            shutil.copyfile(output / name, stage / name)
            (stage / name).chmod(0o755)
        empty = temporary / "empty"
        empty.mkdir(mode=0o700)
        version = subprocess.run(
            [str(stage / "plak"), "version"],
            env=run_environment(empty),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        )
        print("staged_build=" + version.stdout.strip(), flush=True)
        probe = subprocess.run(
            [str(stage / "plak"), "version", "--reload-probe"],
            env=run_environment(empty),
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
            timeout=30,
        )
        key = json.loads(probe.stdout)["state_key"]
        source_key = session_state_key(home, session_id, key)
        if source_key != key:
            return rehearse_carry(
                home, temporary, stage / "plak", session_id, source_key, key,
                options.installed, options.config_overlay, options.wait_idle, options.timeout, options.interval
            )
        backup = temporary / "home"
        return rehearse_copy(
            home, backup, key, stage / "plak", session_id,
            options.wait_idle, options.timeout, options.interval,
        )


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (ValueError, OSError, sqlite3.Error, subprocess.SubprocessError, KeyError, SessionBusy) as error:
        sys.exit(refuse(error))
