"""Canonical stores created by a full test-hooks binary, opened by a reduced binary."""

from __future__ import annotations

import json
import os
from contextlib import ExitStack, closing
from pathlib import Path
import queue
import sqlite3
import subprocess
import tempfile
import threading
import time


def store_bytes(home: Path) -> dict[str, bytes]:
    """Include WALs and all authority bytes; exclude only telemetry and SQLite shm."""
    return {str(path.relative_to(home)): path.read_bytes()
            for path in (home / "state").rglob("*") if path.is_file()
            and "telemetry" not in path.parts and not path.name.endswith("-shm")}


def check(full_binary: str, reduced_binary: str, server_binary: str, maintainer: bool = False) -> list[str]:
    errors = []
    with tempfile.TemporaryDirectory(prefix="plak-feature-store-") as directory:
        root = Path(directory)
        home = root / "home"
        home.mkdir(mode=0o700)
        (home / "agents").mkdir(mode=0o700)
        script = root / "script.json"
        script.write_text(json.dumps({"version": 1, "steps": [{"output": {"text": "fixture answer"}}]}))
        environment = dict(os.environ, PLAK_HOME=str(home), PLAK_TEST_MODEL="offline-loopback-v1",
                           PLAK_TEST_SCRIPT=str(script))
        for name in ("OPENAI_API_KEY", "XAI_API_KEY", "ANTHROPIC_API_KEY", "OPENROUTER_API_KEY"):
            environment.pop(name, None)

        def run(binary, arguments, input=""):
            return subprocess.run([binary, *arguments], cwd=root, env=environment, input=input,
                                  text=True, capture_output=True, check=False, timeout=30)

        for feature in ("python", "mcp"):
            config = '[root]\nagent = "root-agent"\n'
            template = 'account = "codex"\nmodel = "offline-loopback-v1"\neffort = "medium"\n'
            if feature == "python":
                template += 'composition = "py-ptc"\n'
            else:
                config += ('[mcp_servers.fixture]\ntransport = "stdio"\ncommand = '
                           + json.dumps(server_binary) + '\n')
                template += 'tools = ["mcp:fixture"]\n'
            (home / "config.toml").write_text(config)
            (home / "agents/root-agent.toml").write_text(template)
            created = run(full_binary, ["run", "--account", "codex", "--model", "offline-loopback-v1",
                                        "--effort", "medium", "--print-session-id", "--", "create fixture"])
            session = next((line.removeprefix("session_id=") for line in created.stdout.splitlines()
                            if line.startswith("session_id=")), None)
            if created.returncode or session is None:
                errors.append(f"{feature} canonical fixture creation: {created.stdout}{created.stderr}")
                continue
            database = next(home.rglob("product.sqlite3"))
            with closing(sqlite3.connect(f"file:{database}?mode=ro", uri=True)) as connection:
                locator, composition, frozen = connection.execute(
                    "SELECT locator, composition, frozen_tools FROM sessions WHERE id = ?", (session,)
                ).fetchone()
            if not locator or (feature == "python" and composition != "py-ptc"):
                errors.append(f"{feature} fixture did not create the requested canonical binding")
            if feature == "mcp" and (not frozen or "mcp:fixture" not in json.loads(frozen).get("template", {}).get("tools", [])):
                errors.append("MCP fixture did not freeze the selected server tools")
            # Current config must not provide the refusal: the persisted binding must.
            (home / "config.toml").write_text("")
            (home / "agents/root-agent.toml").unlink()
            exported = root / f"{feature}.json"
            cases = [(["session", "show", session], 0),
                     (["session", "log", session], 0),
                     (["session", "export", session, "--out", str(exported)], 0),
                     (["agent", "--session", session], 2),
                     (["run", "--session", session, "--", "refuse"], 2),
                     (["session", "recover", session], 2),
                     (["usage", "--session", session], 2)]
            if feature == "python":
                cases.append((["session", "import", str(exported)], 2))
            if maintainer:
                cases.extend([(["fork", session, "--yes"], 2),
                              (["compact", session, "--yes"], 2)])
            with ExitStack() as readers:
                # Keep WAL readers alive so SQLite's last-connection cleanup
                # cannot add/remove empty WAL files between byte snapshots.
                # Nonempty WAL changes remain part of the comparison.
                for database in (home / "state").rglob("*.sqlite3"):
                    connection = readers.enter_context(closing(sqlite3.connect(f"file:{database}?mode=ro", uri=True)))
                    connection.execute("BEGIN")
                    connection.execute("PRAGMA user_version").fetchone()
                for arguments, code in cases:
                    before = store_bytes(home)
                    result = run(reduced_binary, arguments)
                    if result.returncode != code or (code == 2 and f"feature `{feature}`" not in result.stderr):
                        errors.append(f"{feature} fixture {arguments}: expected exit {code}; {result.returncode}: {result.stdout}{result.stderr}")
                    after = store_bytes(home)
                    if after != before:
                        changed = {name: (len(before.get(name, b"")), len(after.get(name, b"")))
                                   for name in before.keys() | after.keys() if before.get(name) != after.get(name)}
                        errors.append(f"{feature} fixture {arguments}: changed store bytes {changed}")
            if feature == "mcp" and maintainer:
                # Export carries history and composition, not the local tool freeze.
                # Import binds at genesis under the destination config (empty here).
                database = next(home.rglob("product.sqlite3"))
                with closing(sqlite3.connect(f"file:{database}?mode=ro", uri=True)) as connection:
                    source_before = connection.execute("SELECT * FROM sessions WHERE id = ?", (session,)).fetchone()
                    ids = {row[0] for row in connection.execute("SELECT id FROM sessions")}
                result = run(reduced_binary, ["session", "import", str(exported)])
                # The hooks writer provides a deterministic genesis runtime when selected.
                if result.returncode:
                    errors.append(f"MCP import genesis rebind: {result.returncode}: {result.stdout}{result.stderr}")
                with closing(sqlite3.connect(f"file:{database}?mode=ro", uri=True)) as connection:
                    fresh = connection.execute("SELECT id, frozen_tools FROM sessions").fetchall()
                    fresh = [(id, tools) for id, tools in fresh if id not in ids]
                    source_after = connection.execute("SELECT * FROM sessions WHERE id = ?", (session,)).fetchone()
                if source_after != source_before or len(fresh) != 1:
                    errors.append("MCP import genesis must create exactly one successor without editing source")
                elif fresh[0][1] and json.loads(fresh[0][1]).get("template") is not None:
                    errors.append("MCP import carried frozen template tools instead of binding destination genesis")
        # An unused Python template must not prevent either session-running face.
        (home / "config.toml").write_text('[root]\nagent = "root-agent"\n')
        (home / "agents/root-agent.toml").write_text('account = "openrouter"\nmodel = "openai/gpt-6-astra"\ncomposition = "direct"\n')
        (home / "agents/python.toml").write_text('account = "codex"\nmodel = "gpt-6-astra"\ncomposition = "py-ptc"\n')
        environment.pop("PLAK_TEST_MODEL", None)
        for arguments in (["run", "--account", "openrouter", "--model", "openai/gpt-6-astra", "--", "hello"], ["agent"]):
            result = run(reduced_binary, arguments)
            if result.returncode != 3 or "feature `python`" in result.stderr:
                errors.append(f"unused Python template blocked {arguments}: {result.returncode}: {result.stdout}{result.stderr}")
    return errors


def image_diagnostic(binary: str) -> list[str]:
    """Refuse an image during a real held turn, then finish that same turn and keep serving."""
    with tempfile.TemporaryDirectory(prefix="plak-feature-jsonl-") as directory:
        root = Path(directory)
        script = root / "script.json"
        script.write_text(json.dumps({"version": 1, "steps": [{"output": {"text": "same turn finished"}}]}))
        release = root / "release"
        environment = dict(os.environ, PLAK_HOME=str(root / "home"), PLAK_TEST_MODEL="offline-loopback-v1",
                           PLAK_TEST_SCRIPT=str(script), PLAK_TEST_PROVIDER_RELEASE=str(release))
        process = subprocess.Popen([binary, "agent", "--account", "codex", "--model", "offline-loopback-v1",
                                    "--effort", "medium", "--composition", "direct"],
                                   cwd=root, env=environment, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        records = queue.Queue()

        def read():
            for line in process.stdout:
                records.put(json.loads(line))
            records.put(None)

        reader = threading.Thread(target=read, daemon=True)
        reader.start()
        seen = []

        def until(predicate):
            deadline = time.monotonic() + 30
            while True:
                record = records.get(timeout=max(0, deadline - time.monotonic()))
                if record is None:
                    raise RuntimeError(f"unexpected JSONL EOF: {seen}")
                seen.append(record)
                if predicate(record):
                    return record

        def send(value):
            process.stdin.write(json.dumps(dict(v=1, **value)) + "\n")
            process.stdin.flush()

        try:
            hello = until(lambda row: row.get("kind") == "hello")
            expected = ["anthropic", "claude", "codex", "grok", "openai", "openrouter", "xai-api"]
            if hello.get("capabilities") != expected or "python_host" in hello or "python_host_target" in hello:
                raise RuntimeError(f"reduced hello vocabulary/worker status: {hello}")
            until(lambda row: row.get("kind") == "ready")
            send(dict(kind="prompt", id="active", text="run this turn"))
            until(lambda row: row.get("kind") == "turn_lifecycle" and row.get("phase") == "admitted")
            send(dict(kind="prompt", id="image", text="do not admit", images=[dict(path="missing.png")]))
            error = until(lambda row: row.get("kind") == "error" and row.get("id") == "image")
            if "feature `images`" not in error.get("message", ""):
                raise RuntimeError(f"missing image capability refusal: {error}")
            release.touch()
            until(lambda row: row.get("kind") == "turn_lifecycle" and row.get("phase") == "completed")
            if any(row.get("phase") in {"cancelled", "failed"} for row in seen):
                raise RuntimeError(f"image diagnostic disturbed the active turn: {seen}")
            send(dict(kind="tree", id="still-connected"))
            until(lambda row: row.get("kind") in {"tree", "error"} and row.get("id") == "still-connected")
            send(dict(kind="quit", id="quit"))
            process.wait(timeout=30)
            if process.returncode:
                raise RuntimeError(f"JSONL image diagnostic exited {process.returncode}")
        except (RuntimeError, queue.Empty, subprocess.TimeoutExpired, BrokenPipeError) as error:
            return [f"JSONL image diagnostic: {type(error).__name__}: {error}; last records: {seen[-8:]}"]
        finally:
            release.touch()
            if process.poll() is None:
                process.kill()
            process.wait(timeout=30)
            reader.join(timeout=5)
            process.stdin.close()
            process.stdout.close()
            process.stderr.close()
    return []
