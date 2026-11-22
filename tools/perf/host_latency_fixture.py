"""Generate the latency workload through the actual host, without copied state."""

import asyncio
import contextlib
import json
import os
import signal
import sqlite3
import uuid

from host_latency_wire import LOGIN, ORIGIN

MODEL = "offline-loopback-v1"
TOOL_BATCH = 7


def tool(name, **arguments):
    return {"output": {"tool": {"name": name, "arguments": arguments}}, "finish": "tool_calls"}


def answer(text):
    return {"output": {"text": text}}


class Fixture:
    def __init__(self, root, binary):
        self.root = root
        self.binary = binary
        self.home = root / "home"
        self.socket = root / "w.sock"
        self.gateway = root / "g.sock"
        self.workspace = root / "workspace"
        self.script = root / "script.json"
        self.diagnostics = self.home / "logs/plak.log"
        self.processes = []

    def init(self):
        if len(os.fsencode(self.root / "home/host/runner/vXX-000000000000/run/r.sock")) > 100:
            raise ValueError("scratch path is too long for a Unix socket")
        self.root.mkdir(mode=0o700)
        for directory in ("home", "home/agents", "workspace", "os", "tmp", "ui"):
            (self.root / directory).mkdir(mode=0o700)
        (self.home / "config.toml").write_text(
            '[root]\nagent = "lead"\n'
            '\n[orchestration]\nsubagents = false\n'
            '\n[compaction]\nenabled = false\n'
            f'\n[web]\nallow = ["{LOGIN}"]\norigin = "{ORIGIN}"\n'
            'accounts_read = true\nconfig_read = true\n'
            f'\n[gateway]\naddress = {json.dumps(f"unix:{self.gateway}")}\n'
        )
        (self.home / "gateway.toml").write_text(f"socket = {json.dumps(str(self.gateway))}\n")
        (self.home / "agents/lead.toml").write_text(
            'account = "codex"\n'
            f'model = "{MODEL}"\n'
            'effort = "medium"\ncomposition = "direct"\ndescription = "Synthetic latency root"\n'
        )
        (self.root / "ui/index.html").write_text("<!doctype html><title>Synthetic probe</title>")
        csp = (
            "default-src 'self'; script-src 'self'; connect-src 'self'; "
            "require-trusted-types-for 'script'; frame-ancestors 'none'; object-src 'none'; base-uri 'none'"
        )
        (self.root / "ui/csp.json").write_text(json.dumps({"header": csp}))

    def write_script(self, steps, **options):
        temporary = self.script.with_suffix(".new")
        with temporary.open("w") as output:
            json.dump({"version": 1, "steps": steps, **options}, output)
        temporary.replace(self.script)

    async def spawn(self, *args, pipes=False):
        # Do not inherit credentials, native homes, git config or socket selectors.
        env = {
            "PATH": os.environ["PATH"],
            "HOME": str(self.root / "os"),
            "XDG_STATE_HOME": str(self.root / "os"),
            "TMPDIR": str(self.root / "tmp"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "PLAK_HOME": str(self.home),
            "PLAK_TEST_MODEL": MODEL,
            "PLAK_TEST_SCRIPT": str(self.script),
            "PLAK_TEST_EVIDENCE": str(self.root / "model.jsonl"),
            "PLAK_TEST_FAULTS": "fault=connect",
            "PLAK_LOG": "plak::web=info",
        }
        devnull, pipe = asyncio.subprocess.DEVNULL, asyncio.subprocess.PIPE
        with (self.root / f"{args[0]}.log").open("ab") as log:
            child = await asyncio.create_subprocess_exec(
                str(self.binary),
                *args,
                cwd=self.workspace,
                env=env,
                start_new_session=True,
                stdin=pipe if pipes else devnull,
                stdout=pipe if pipes else devnull,
                stderr=log,
                limit=64 << 20,
            )
        self.processes.append(child)
        return child

    async def stop(self, child):
        if child.stdin:
            child.stdin.close()
        if child.returncode is None:
            try:
                child.send_signal(signal.SIGINT)
            except ProcessLookupError:
                pass
            try:
                await asyncio.wait_for(child.wait(), 20)
            except TimeoutError:
                if child.returncode is None:
                    os.killpg(child.pid, signal.SIGKILL)
                await child.wait()
        self.processes.remove(child)

    async def close(self):
        for child in reversed(self.processes[:]):
            await self.stop(child)

    async def serve(self):
        gateway = await self.spawn("gateway", "serve")
        await self.ready(gateway, self.gateway.exists)
        # Own the local runner as a child rather than letting the host launch a
        # detached daemon that would survive the measurement.
        runner = await self.spawn("runner", "serve", "--local")
        await self.ready(runner, lambda: any(self.home.glob("host/runner/*/run/r.sock")))
        host = await self.spawn(
            "host",
            "--web",
            f"unix:{self.socket}",
            "--ui",
            str(self.root / "ui"),
        )
        log = self.root / "host.log"
        await self.ready(host, lambda: "plakd host: serving on " in log.read_text())
        return host.pid

    @staticmethod
    async def ready(child, predicate):
        async with asyncio.timeout(60):
            while not predicate():
                if child.returncode is not None:
                    raise RuntimeError(
                        f"scratch process exited {child.returncode} before readiness"
                    )
                await asyncio.sleep(0.05)

    def database(self, name):
        (database,) = (self.home / "state").glob(f"*/{name}.sqlite3")
        return database

    def query(self, name, sql, *params):
        uri = self.database(name).as_uri() + "?mode=ro"
        with contextlib.closing(sqlite3.connect(uri, uri=True)) as db:
            return db.execute(sql, params).fetchone()

    def counts(self, node):
        tree = node["tree_id"]
        (roots,) = self.query("product", "select count(*) from sessions")
        rows, size = self.query(
            "product",
            "select count(*), sum(length(row)) from transcript_rows where session_id=?",
            tree,
        )
        children, active, prompt_bytes = self.query(
            "subagents",
            "select count(*), sum(status='running'), sum(length(prompt)) from agent_nodes "
            "where tree_id=? and parent_agent_id is not null",
            tree,
        )
        return dict(
            roots=roots,
            retained_children=children,
            active_children=active or 0,
            child_prompt_bytes=prompt_bytes or 0,
            transcript_rows=rows,
            transcript_row_bytes=size,
        )

    async def witness_wait(self, node, minimum_children):
        tree = node["tree_id"]
        async with asyncio.timeout(60):
            while True:
                (waits,) = self.query(
                    "product",
                    "select count(*) from session_call_views as c join transcript_rows as r "
                    "on c.session_id=r.session_id and c.call_id=r.call_id where c.session_id=? "
                    "and json_extract(r.row,'$.name')='wait_agent' "
                    "and json_extract(c.value,'$.settled')=0",
                    tree,
                )
                counts = self.counts(node)
                (open_turns,) = self.query(
                    "product",
                    "select count(*) from session_open_turn_views where session_id=?",
                    tree,
                )
                if waits and counts["active_children"] and open_turns:
                    if counts["retained_children"] != minimum_children:
                        raise RuntimeError(f"wrong retained child count: {counts}")
                    return {**counts, "open_root_turns": open_turns, "open_waits": waits}
                await asyncio.sleep(0.1)


def tree_script(history_turns, children, child_bytes):
    history = [answer(f"synthetic reply {i}") for i in range(history_turns)]
    settled = []
    for start in range(0, children - 1, TOOL_BATCH):
        batch = range(start, min(start + TOOL_BATCH, children - 1))
        settled.extend(spawn(i, child_bytes) for i in batch)
        settled.append(tool("wait_agent", agent_ids=[f"c{i + 1}" for i in batch], timeout_ms=1000))
        settled.append(answer("children settled"))
    held = spawn(children - 1, child_bytes)
    wait = tool("wait_agent", agent_ids=[f"c{children}"], timeout_ms=3_600_000)
    return history + settled + [held, wait], (children + TOOL_BATCH - 2) // TOOL_BATCH


def spawn(index, child_bytes):
    return tool(
        "spawn_agent",
        template="implement",
        tier="mutate",
        workspace="fork",
        description=f"synthetic child {index + 1}",
        prompt=f"synthetic child {index + 1}: " + "s" * child_bytes,
        output_schema_json='{"type":"object","properties":{"summary":{"type":"string"}},'
        '"required":["summary"],"additionalProperties":false}',
    )


async def create(client, workspace):
    reply, _, _ = await client.request(
        {
            "op": "new_session",
            "runner": {"name": "local", "runner_id": "local"},
            "workspace": str(workspace),
            "request_id": str(uuid.uuid4()),
            "accounts": [],
        }
    )
    return reply["node"]


async def turn(client, state, message):
    terminal = state.terminal
    reply, _, _ = await client.intent(message)
    if reply["answer"] != "accepted":
        raise RuntimeError(f"seed intent was not accepted: {reply}")
    await client.wait(lambda: state.terminal > terminal)
    if state.phase != "completed":
        raise RuntimeError(f"seed turn did not complete: {state.phase}")


async def populate(fixture, args, terminal, batches):
    # Grow the big root before making the directory large; the probe, not
    # setup, measures repeated directory scans with a full fixture.
    heavy = await create(terminal, fixture.workspace)
    state, _ = await terminal.attach(heavy)
    for i in range(args.history_turns):
        await turn(terminal, state, f"synthetic history {i}: " + "x" * args.prompt_bytes)
    for i in range(batches):
        await turn(terminal, state, f"retain synthetic children batch {i}")
    others = [await create(terminal, fixture.workspace) for _ in range(args.roots - 1)]
    return heavy, state, others[0]


async def start_fixture(fixture, args):
    steps, batches = tree_script(args.history_turns, args.children, args.child_bytes)
    fixture.write_script(
        steps,
        tree=True,
        durable_tree=True,
        child_hold_ms=3_600_000,
        child_hold_after=args.children - 1,
    )
    return await fixture.serve(), batches
