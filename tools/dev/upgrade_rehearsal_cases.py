"""Real-process bootstrap and simulated-key dormant ownership-move cases."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import struct
import subprocess
import uuid

# Increment-4 live/parked-tree cases register (name, dormant, body) here later.
INCREMENT4_CASES = ()


def terminal_inventory(home, line, support):
    """Observe frozen retention-v1 Lines only: never bind native ownership or ack jobs."""
    observations = {}
    host_id = (home / "host-id").read_text().strip()
    for path in sorted((home / "builds").glob("*/retention-v1.sock")):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
            stream.settimeout(10)
            directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                stream.connect(f"/proc/self/fd/{directory}/{path.name}")
            finally:
                os.close(directory)
            reader = stream.makefile("rb")
            with reader:

                def receive():
                    prefix = reader.read(4)
                    support.require(len(prefix) == 4, "retention probe closed before length")
                    length = struct.unpack(">I", prefix)[0]
                    support.require(0 < length <= 1024 * 1024, "retention probe frame bound")
                    body = reader.read(length)
                    support.require(len(body) == length, "retention probe truncated frame")
                    return json.loads(body)

                def send(value):
                    body = json.dumps(value).encode()
                    stream.sendall(struct.pack(">I", len(body)) + body)

                origin = receive()["origin"]
                send({"version": 1, "host_id": host_id, "host_instance": str(uuid.uuid4())})
                support.require(receive() == {"answer": "ok"}, "retention probe bind refused")
                after = 0
                for page in range(64):
                    request_id = f"rehearsal-lines-{page}"
                    send(
                        {
                            "id": request_id,
                            "instance": origin["instance"],
                            "mutation": None,
                            "request": {"verb": "lines", "after": after},
                        }
                    )
                    reply = receive()
                    support.require(reply["id"] == request_id, "retention probe correlation")
                    answer = reply["answer"]
                    support.require(
                        answer["answer"] == "lines", f"retention probe refused: {answer}"
                    )
                    for row in answer["lines"]:
                        if row["owner"]["line"] != line:
                            continue
                        for job in row["jobs"]:
                            support.require(
                                job["number"] == 1
                                and job["terminal"]
                                and job["status"] == {"status": "exited", "code": 0}
                                and job["output"] == "retained-rehearsal-output"
                                and row["next_number"] == 2,
                                "original-worker unconsumed terminal/output/number floor changed",
                            )
                            identity = (row["origin"]["instance"], job["session"])
                            observations[identity] = {
                                "origin": row["origin"],
                                "session": job["session"],
                                "number": job["number"],
                                "output": job["output"],
                                "next_number": row["next_number"],
                            }
                    next_page = answer["next"]
                    if next_page is None:
                        break
                    support.require(next_page > after, "retention probe cursor did not advance")
                    after = next_page
                else:
                    raise support.Refused("retention probe page bound")
    support.require(len(observations) == 1, "the unconsumed terminal job disappeared or duplicated")
    return list(observations.values())


def history_content(rows):
    # Presentation-entry keys are checkpoint-derived, not canonical message IDs.
    return [{key: value for key, value in row.items() if key != "entry_id"} for row in rows]


def operator_summary(report):
    lines = ["# Cross-key rehearsal", ""]
    for name in ["A", "B"]:
        image = report["images"][name]
        lines.append(f"{name}: `{image['revision']}` / binary `{image['binary_id']}`.")
    lines += [
        "",
        "BOOTSTRAP uses the actual main and candidate codecs. DORMANT uses the candidate image with a simulated old key; it does not qualify old-codec compatibility.",
        "",
        "| Case | Result | Operator outcome / remedy |",
        "| --- | --- | --- |",
    ]
    for row in report["cases"]:
        detail = (
            row.get("error")
            or row.get("operator_remedy")
            or row.get("outcome")
            or "history, named state, paused queue and identity checks passed"
        )
        detail = detail.replace("\n", " ").replace("|", "\\|")
        lines.append(f"| {row['case']} | {row['status']} | {detail} |")
        if row.get("mappings"):
            mappings = row["mappings"]
            if isinstance(mappings, list):
                mappings = {item["source"]: item["successor"] for item in mappings}
            for source, destination in mappings.items():
                lines.append(f"| | | `{source}` → `{destination}` |")
    lines += [
        "",
        "Bootstrap settles into NEW session/tree/native-line identities. Canonical export history and named state are checked exactly; presentation-entry IDs are not canonical identity. Text queues stay paused. Completed children are only source facts: read their old history with A; B cannot resume them. Save terminal output before bootstrap: an exit notice can carry in history, but the job ledger does not.",
        "",
        report["increment4"],
        "",
    ]
    failed = any(row["status"] != "passed" for row in report["cases"])
    lines.append(
        "NOT QUALIFIED: at least one case failed; do not enable dormant ownership moves."
        if failed
        else "Selected cases passed. This is scratch evidence, not production enablement or coverage of omitted cases."
    )
    return "\n".join(lines) + "\n"


class Case:
    def __init__(self, support, root, name, images, dormant, child_mode="completed"):
        self.s = support
        self.root = root
        self.name = name
        self.path = root / "owned" / "cases" / name
        self.path.mkdir(parents=True, mode=0o700)
        self.env = support.environment(self.path)
        self.images = images
        self.dormant = dormant
        self.child_mode = child_mode
        self.source = "B" if dormant else "A"
        self.children = []
        self.host = None
        self.cut = None
        self.workspace = self.path / "workspace"
        self.workspace.mkdir()
        # Bound project discovery before it can reach an operator ancestor.
        for directory in [self.path, self.workspace]:
            project = directory / ".plak"
            project.mkdir()
            (project / "config.toml").write_text("")
        git = shutil.which("git")
        support.require(git, "git is required to seed the scratch workspace")
        support.run([git, "init", "-q", str(self.workspace)], env=self.env)
        support.run(
            [
                git,
                "-C",
                str(self.workspace),
                "-c",
                "user.name=Rehearsal",
                "-c",
                "user.email=rehearsal@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "scratch",
            ],
            env=self.env,
        )
        home = Path(self.env["PLAK_HOME"])
        (home / "agents").mkdir()
        (home / "config.toml").write_text(
            '[root]\nagent = "lead"\n[orchestration]\nsubagents = false\n[compaction]\nenabled = false\n'
        )
        template = 'account = "codex"\nmodel = "offline-loopback-v1"\neffort = "medium"\ncomposition = "direct"\ndescription = "offline rehearsal"\n'
        for name in ["lead", "review", "explore", "implement", "orchestrate", "note"]:
            (home / "agents" / f"{name}.toml").write_text(template)
        spec = importlib.util.spec_from_file_location(
            "rehearsal_host_client", images["checkout"] / "tools/examples/host_client.py"
        )
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.client_type = module.Client
        self.script("idle")
        probes = {}
        for role, expected in [
            ("source", support.SOURCE_KEY),
            ("destination", support.DESTINATION_KEY),
        ]:
            probe = json.loads(self.command(role, ["version", "--host-upgrade-probe"]).stdout)
            support.require(
                probe["state_key"] == expected,
                f"{role}: unexpected state key; retain the requested revision and test-hooks build",
            )
            probes[role] = probe
        if not dormant:
            support.check_probe_shape(probes["source"], probes["destination"])
            print("PROBE: destination JSON fits the source's strict probe shape", flush=True)

    def command(self, role, arguments, *, check=True, extra=None, timeout=120):
        env = dict(self.env)
        if role == "source" and self.dormant:
            env["PLAK_TEST_STATE_KEY"] = self.s.SOURCE_KEY
        if extra:
            env.update(extra)
        image = self.images[self.source if role == "source" else "B"]["binary"]
        return self.s.run(
            [image, *arguments], cwd=self.workspace, env=env, check=check, timeout=timeout
        )

    def state(self, role):
        return (
            Path(self.env["PLAK_HOME"])
            / "state"
            / (self.s.SOURCE_KEY if role == "source" else self.s.DESTINATION_KEY)
        )

    def socket(self, role):
        key = self.s.SOURCE_KEY if role == "source" else self.s.DESTINATION_KEY
        path = Path(self.env["PLAK_HOME"]) / "host/run" / f"{key}.sock"
        self.s.require(
            len(bytes(path)) < 108,
            "scratch path is too long for a Unix socket; choose a shorter --root",
        )
        return path

    def client(self, role="source"):
        client = self.client_type(str(self.socket(role)), timeout=90)
        client.hello()
        return client

    def start(self, cut=None):
        cut = cut or self.cut
        env = dict(self.env)
        if self.dormant:
            env["PLAK_TEST_STATE_KEY"] = self.s.SOURCE_KEY
        if cut:
            env["PLAK_TEST_HOST_UPGRADE_CUT"] = cut
        log = self.path / f"host-{len(self.children)}.log"
        with log.open("w") as output:
            self.host = subprocess.Popen(
                [self.images[self.source]["binary"], "host"],
                cwd=self.workspace,
                env=env,
                stdout=output,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        self.children.append(self.host)

        def ready():
            self.s.require(self.host.poll() is None, f"source exited: {log.read_text()}")
            return "plak host: serving on " in log.read_text()

        self.s.wait_for(ready, "source host ready")

        def runner_ready():
            with self.client() as client:
                options = client.request(90, {"op": "session_options"})["options"]
                return any(
                    row["runner"]["runner_id"] == "local" and row["status"]["status"] == "ready"
                    for row in options["runners"]
                )

        self.s.wait_for(runner_ready, "home-local runner ownership reconciliation")

    def stop(self):
        if self.host and self.host.poll() is None:
            self.host.send_signal(signal.SIGTERM)
            self.s.require(self.host.wait(timeout=60) == 0, "source host did not stop cleanly")
        self.host = None

    def script(self, kind):
        child = self.path / "child.json"
        if self.child_mode == "completed":
            self.s.write_json(self.path / "grandchild-script.json", {"version": 1, "steps": []})
        self.s.write_json(
            child,
            {
                "version": 1,
                "steps": [
                    {
                        "stream": [
                            {"text": "real child ready"},
                            {"gate": {"file": str(self.path / "child-release")}},
                        ],
                        "output": {
                            "tool": {
                                "name": "submit_result",
                                "arguments": {
                                    "turn_token": 1,
                                    "output_json": '{"summary":"real child result"}',
                                },
                            }
                        },
                        "finish": "tool_calls",
                    }
                ],
            },
        )
        steps = [{"output": {"text": "rehearsal idle"}}]
        if kind == "child":
            steps.insert(
                0,
                {
                    "output": {
                        "tool": {
                            "name": "spawn_agent",
                            "arguments": {
                                "template": "review",
                                "tier": "read_only",
                                "description": "rehearsal child",
                                "prompt": "return one result",
                                "output_schema_json": '{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"],"additionalProperties":false}',
                            },
                        }
                    },
                    "finish": "tool_calls",
                },
            )
        if kind == "job":
            release = str(self.path / "job-release").replace("'", "'\\''")
            done = str(self.path / "job-done").replace("'", "'\\''")
            steps.insert(
                0,
                {
                    "output": {
                        "tool": {
                            "name": "process_session_start",
                            "arguments": {
                                "command": f"while ! test -f '{release}'; do sleep 0.05; done; printf retained-rehearsal-output; printf done > '{done}'",
                                "workdir": ".",
                                "login": False,
                                "tty": False,
                                "keep_stdin_open": False,
                                "yield_time_ms": 0,
                                "max_output_bytes": 4096,
                            },
                        }
                    },
                    "finish": "tool_calls",
                },
            )
        self.s.write_json(
            self.path / "script.json",
            {
                "version": 1,
                "tree": True,
                "durable_tree": True,
                "tree_process_tools": True,
                "product_child_script": str(child),
                "steps": steps,
            },
        )

    def rows(self, role, query, database="product", parameters=()):
        return self.s.read_rows(self.state(role) / f"{database}.sqlite3", query, parameters)

    def idle(self, session, minimum=1, role="source"):
        views = self.rows(
            role,
            "SELECT root_turn_count FROM session_views WHERE session_id=?",
            parameters=(session,),
        )
        return (
            not self.rows(
                role,
                "SELECT * FROM session_open_turn_views WHERE session_id=?",
                parameters=(session,),
            )
            and views
            and views[0]["root_turn_count"] >= minimum
        )

    def seed(self):
        document = json.loads(
            (
                self.images["checkout"] / "bin/plak/tests/fixtures/session-export-v12.json"
            ).read_text()
        )
        document["source"]["session_id"] = str(uuid.uuid4())
        document["workspace_root"] = str(self.workspace)
        document["named_state"] = {"rehearsal": {"answer": 42, "nested": [True, "kept"]}}
        document["queue"] = ["paused text must not execute during the move"]
        path = self.path / "seed.json"
        self.s.write_json(path, document)
        result = self.command("source", ["session", "import", str(path)])
        match = re.search(r"resume with: plak --resume (\S+)", result.stdout)
        self.s.require(match, f"import did not return a session: {result.stdout}")
        return match[1]

    def create(self, kind, *, release=True):
        self.script(kind)
        self.start()
        with self.client() as client:
            created = client.request(
                1,
                {
                    "op": "new_session",
                    "runner": {"name": "local", "runner_id": "local"},
                    "profile": None,
                    "model": None,
                    "template": None,
                    "workspace": str(self.workspace),
                    "request_id": str(uuid.uuid4()),
                },
            )
            attachment = client.request(2, {"op": "attach", "node": created["node"]})["attachment"]
            client.request(
                3,
                {
                    "op": "intent",
                    "attachment": attachment,
                    "client_id": str(uuid.uuid4()),
                    "blobs": [],
                    "intent": {
                        "kind": "prompt",
                        "body": {"text": f"rehearse {kind}", "images": []},
                    },
                },
            )
            self.s.wait_for(lambda: self.idle(created["session"]), f"{kind}: root idle")
            client.request(4, {"op": "detach", "attachment": attachment})
        if kind == "child":
            self.s.wait_for(
                lambda: self.rows(
                    "source",
                    "SELECT agent_id FROM agent_nodes WHERE parent_agent_id IS NOT NULL AND session_locator IS NOT NULL AND fold_order IS NOT NULL",
                    "subagents",
                ),
                "real bound child",
            )
        if kind in {"child", "job"} and release:
            (self.path / f"{kind}-release").touch()
            if kind == "child":
                self.s.wait_for(
                    lambda: self.idle(created["session"], minimum=2),
                    "child result woke its idle root",
                )
                self.s.wait_for(
                    lambda: (
                        not self.rows(
                            "source", "SELECT * FROM agent_nodes WHERE active_turn<>0", "subagents"
                        )
                    ),
                    "child settled",
                )
            else:
                self.s.wait_for(
                    lambda: (self.path / "job-done").exists(),
                    "real process reached terminal output",
                )
        if release and kind != "job":
            self.stop()
        return created["session"]

    def exports(self, role, sessions):
        documents = {}
        for session in sessions:
            path = self.path / f"export-{role}-{session}.json"
            self.command(role, ["session", "export", session, "--out", str(path)])
            documents[session] = json.loads(path.read_text())
        return documents

    def snapshot(self, role):
        result = {
            "products": self.rows(
                role, "SELECT id,node_tree_id,node_agent_id,frozen_tools FROM sessions ORDER BY id"
            ),
            "branches": self.rows(
                role,
                "SELECT session_id,branch_id,line_id FROM branches ORDER BY session_id,branch_id",
                "runtime",
            ),
            "nodes": self.rows(
                role,
                "SELECT tree_id,agent_id,parent_agent_id,template,tuple_id,account,effort,tier,workspace_id,output_schema_json,schema_digest,fold_order,line_id FROM agent_nodes ORDER BY tree_id,agent_id",
                "subagents",
            ),
            "terminals": self.rows(
                role,
                "SELECT line_id,hex(generation) AS generation,job,hex(evidence) AS evidence FROM line_job_terminals ORDER BY line_id,job",
                "runtime",
            ),
        }
        for row in result["products"]:
            if row["frozen_tools"]:
                row["frozen_tools"] = json.loads(row["frozen_tools"])
        return result

    def child_history(self, role, root, child):
        result = self.command(
            role, ["session", "log", f"{root}.{child}", "--json", "--tail", "10000"], check=False
        )
        return {
            "exit": result.returncode,
            "rows": history_content([json.loads(line) for line in result.stdout.splitlines()])
            if result.returncode == 0
            else [],
            "remedy": result.stderr,
        }

    def event_log(self, role, session):
        result = self.command(role, ["session", "log", session, "--json", "--tail", "10000"])
        return history_content([json.loads(line) for line in result.stdout.splitlines()])

    def attach_verify(self, session):
        with self.client("destination") as client:
            rows = client.request(1, {"op": "list_sessions", "include_archived": False})["rows"]
            node = next(row["node"] for row in rows if row["session"] == session)
            attached = client.request(2, {"op": "attach", "node": node})
            self.s.require(attached.get("answer") == "attached", "destination session did not open")
            client.request(3, {"op": "detach", "attachment": attached["attachment"]})
        self.s.wait_for(
            lambda: self.rows(
                "destination",
                "SELECT locator FROM sessions WHERE id=? AND locator IS NOT NULL",
                parameters=(session,),
            ),
            "carried canonical head materialized",
        )

    def interrupt_child(self, session):
        child = self.rows(
            "source",
            "SELECT agent_id FROM agent_nodes WHERE parent_agent_id IS NOT NULL",
            "subagents",
        )[0]["agent_id"]
        with self.client() as client:
            node = next(
                row["node"]
                for row in client.request(1, {"op": "list_sessions", "include_archived": False})[
                    "rows"
                ]
                if row["session"] == session
            )
            attachment = client.request(2, {"op": "attach", "node": node})["attachment"]
            client.request(
                3,
                {
                    "op": "intent",
                    "attachment": attachment,
                    "blobs": [],
                    "intent": {"kind": "interrupt_topology_node", "body": {"agent_id": child}},
                },
            )
            self.s.wait_for(
                lambda: (
                    not self.rows(
                        "source", "SELECT * FROM agent_nodes WHERE active_turn<>0", "subagents"
                    )
                ),
                "interrupted bound child quiescent",
            )
            client.request(4, {"op": "detach", "attachment": attachment})
        self.stop()

    def receipts(self):
        paths = Path(self.env["PLAK_HOME"]).glob("host/upgrades/*/receipt.json")
        return sorted(
            (json.loads(path.read_text()) for path in paths), key=lambda row: row["sequence"]
        )

    def upgrade(self, cut=None):
        extra = {"PLAK_TEST_HOST_UPGRADE_CUT": cut} if cut else None
        probe = json.loads(self.command("destination", ["version", "--host-upgrade-probe"]).stdout)
        self.s.require(
            probe["state_key"] == self.s.DESTINATION_KEY,
            "destination is not the requested natural key",
        )
        return self.command(
            "destination",
            ["host", "upgrade", "--expect-binary", probe["binary_id"]],
            check=False,
            extra=extra,
        )

    def persist_logs(self):
        for path in self.path.glob("*.log"):
            (self.root / f"{self.name}-{path.name}").write_bytes(path.read_bytes())


def carry(case, *, cut=None, jobs=True):
    case.cut = cut
    seeded = case.seed()
    child_root = case.create("child", release=case.child_mode == "completed")
    if case.child_mode != "completed":
        case.interrupt_child(child_root)
    sessions = [seeded, child_root]
    ordinary = list(sessions)
    job_session = case.create("job") if jobs else None
    if job_session:
        sessions.append(job_session)
    case.script("idle")
    before = case.exports("source", ordinary)
    histories = {session: case.event_log("source", session) for session in sessions}
    original = case.snapshot("source")
    child = next(row["agent_id"] for row in original["nodes"] if row["parent_agent_id"])
    child_before = case.child_history("source", child_root, child)
    case.s.require(
        child_before["exit"] == 0 and child_before["rows"],
        "source child has no readable canonical history",
    )
    if not case.host:
        case.start(cut)
    result = case.upgrade(cut)
    if cut:
        case.s.require(result.returncode != 0, f"crash cut was not reached: {cut}")
        case.s.wait_for(lambda: case.host.poll() is not None, "cut source process exited")
        case.s.require(
            case.host.returncode == 86,
            f"cut did not produce the explicit hook exit: {case.host.returncode}",
        )
        prepared = case.receipts()[-1]["phase"] == "prepared"
        case.cut = None
        if prepared:
            case.start()
            case.s.require(
                case.receipts()[-1]["phase"] == "aborted", "source did not withdraw prepared move"
            )
            case.s.require(
                case.snapshot("source") == original,
                "prepared cut changed source identities or bindings",
            )
        result = case.upgrade()
    case.s.require(result.returncode == 0, f"upgrade refused:\n{result.stderr}\n{result.stdout}")
    authority = case.receipts()[-1]
    case.s.require(
        authority["phase"] == "acknowledged", "destination did not acknowledge ownership"
    )
    mappings = {row["source"]: row["successor"] for row in authority["mappings"]}
    case.s.require(set(mappings) == set(sessions), "move silently omitted a source session")
    for target in mappings.values():
        case.attach_verify(target)
    after = case.exports("destination", [mappings[session] for session in ordinary])
    for source, target in mappings.items():
        if source in before:
            for field in ["history", "named_state", "queue"]:
                case.s.require(
                    before[source][field] == after[target][field], f"{field} changed for {source}"
                )
        refused = case.command(
            "source", ["run", "--session", source, "--", "must not execute"], check=False
        )
        case.s.require(
            refused.returncode != 0
            and any(code in refused.stderr for code in ["source_fenced", "session_archived"]),
            f"old source was not fenced: {refused.stderr}",
        )
    report = {
        "mappings": mappings,
        "queue": "text preserved and paused",
        "old_source": "refused",
        "history": "root histories and named state identical",
    }
    if case.dormant:
        native = {
            row["operation"].lower(): row["digest"].lower()
            for row in case.rows(
                "destination",
                "SELECT hex(operation) AS operation,hex(digest) AS digest FROM runtime_moves",
                "runtime",
            )
        }
        job_ledgers = []
        for group in authority["groups"]:
            case.s.require(
                native.get(bytes(group["operation"]).hex()) == group["target_digest"],
                "published native digest differs from the reviewed reconstructed group (history/bindings/jobs)",
            )
            path = (
                Path(case.env["PLAK_HOME"])
                / "host/upgrades"
                / authority["id"]
                / f"{group['id']}.json"
            )
            capsule = json.loads(path.read_text())
            job_ledgers.extend(
                head["jobs"]
                for session in capsule["sessions"]
                for head in session["heads"]
                if head["jobs"]["rows"]
            )
        if jobs:
            case.s.require(
                job_ledgers
                and any(
                    row["terminal"]["tail"].find("retained-rehearsal-output") >= 0
                    for ledger in job_ledgers
                    for row in ledger["rows"]
                ),
                "no retained terminal output in the source group",
            )
            case.s.require(
                all(
                    ledger["next_number"] > max(row["number"] for row in ledger["rows"])
                    for ledger in job_ledgers
                ),
                "retained job number floor regressed",
            )
        case.s.write_json(
            case.root / f"{case.name}-invariants.json",
            {
                "before": original,
                "after": case.snapshot("destination"),
                "history_before": histories,
                "jobs": job_ledgers,
                "groups": authority["groups"],
            },
        )
        case.s.require(
            all(source == target for source, target in mappings.items()),
            "dormant move allocated successor IDs",
        )
        case.s.require(
            case.snapshot("destination") == original,
            "dormant move changed tree, child, native line, frozen tools or retained job evidence",
        )
        case.s.require(
            case.child_history("destination", child_root, child) == child_before,
            "dormant child canonical history changed",
        )
        case.s.require(
            authority["groups"]
            and all(
                group["phase"] == "ready" and group["source_retired"]
                for group in authority["groups"]
            ),
            "group did not retire and publish Ready",
        )
        report.update(
            child="bound child identity/history preserved",
            jobs="terminal IDs/evidence preserved"
            if jobs
            else "not present; repro-job tests retained terminals",
            keys="simulated old key; both images use candidate codecs",
        )
        if case.child_mode == "completed":
            wake_after_move(case, child_root, child)
            report["wake"] = "next child result woke the moved idle root once"
    else:
        case.s.require(
            all(source != target for source, target in mappings.items()),
            "bootstrap unexpectedly claimed dormant preserved IDs",
        )
        document = after[mappings[child_root]]
        retained_export = (
            Path(case.env["PLAK_HOME"]) / "host/upgrades" / authority["id"] / f"{child_root}.json"
        )
        source_facts = json.loads(retained_export.read_text())
        report.update(
            child={
                "source_tree": next(
                    row["node_tree_id"] for row in original["products"] if row["id"] == child_root
                ),
                "successor_tree": next(
                    row["node_tree_id"]
                    for row in case.snapshot("destination")["products"]
                    if row["id"] == mappings[child_root]
                ),
                "inert_facts": source_facts["pending_work"],
                "original_child_count": source_facts["source_subagents"],
                "old_child_history_read_only": case.child_history("source", child_root, child)
                == child_before,
                "source_subagents": document.get("source_subagents"),
                "source_resolved_count": document.get("source_resolved_count"),
                "destination_log": case.child_history("destination", mappings[child_root], child),
            },
            jobs="not included in successful bootstrap fixture",
            keys="real main codec to real candidate codec",
        )
    return report


def bootstrap_observation(case, kind):
    session = case.create(
        "child" if kind == "active-child" else "job", release=kind == "terminal-job"
    )
    if case.host is None:
        case.start()
    before = case.snapshot("source")
    history_before = case.event_log("source", session)
    result = case.upgrade()
    receipts = case.receipts()
    if result.returncode:
        if kind == "active-child":
            expected = "host_upgrade_blocked" in result.stderr and "live_child" in result.stderr
        elif kind == "active-job":
            expected = "host_upgrade_process_retention_unavailable" in result.stderr
        else:
            expected = "host_upgrade_blocked" in result.stderr and "daemon_jobs" in result.stderr
        case.s.require(
            expected, f"unexpected bootstrap refusal, not a qualified blocker: {result.stderr}"
        )
        case.s.require(
            not any(row["phase"] in {"committed", "acknowledged"} for row in receipts),
            "refusal surrendered a source",
        )
        with case.client() as client:
            client.request(1, {"op": "ping"})
            reopened = client.request(
                2,
                {
                    "op": "new_session",
                    "runner": {"name": "local", "runner_id": "local"},
                    "workspace": str(case.workspace),
                    "request_id": str(uuid.uuid4()),
                },
            )
            case.s.require(
                reopened.get("answer") == "created",
                "source admission remained closed after refusal",
            )
        return {
            "session": session,
            "outcome": "refused before surrender",
            "operator_remedy": result.stderr,
            "exit": result.returncode,
            "source_admission": "reopened; a new dormant session was accepted without inference",
        }
    case.s.require(kind == "terminal-job", f"bootstrap unexpectedly carried {kind}")
    authority = receipts[-1]
    mapping = next(row for row in authority["mappings"] if row["source"] == session)
    case.attach_verify(mapping["successor"])
    refused = case.command(
        "source", ["run", "--session", session, "--", "must not execute"], check=False
    )
    case.s.require(
        refused.returncode != 0
        and any(code in refused.stderr for code in ["source_fenced", "session_archived"]),
        "old job session did not refuse after bootstrap",
    )
    source_export = json.loads(
        (
            Path(case.env["PLAK_HOME"]) / "host/upgrades" / authority["id"] / f"{session}.json"
        ).read_text()
    )
    target_export = case.exports("destination", [mapping["successor"]])[mapping["successor"]]
    case.s.require(
        source_export["history"] == target_export["history"],
        "settled terminal-job conversation changed",
    )
    case.s.require(
        "retained-rehearsal-output" in json.dumps(target_export["history"]),
        "bootstrap carried the job without delivering its fixture output",
    )
    after = case.snapshot("destination")
    case.s.write_json(
        case.root / f"{case.name}-invariants.json",
        {
            "before": before,
            "after": after,
            "history_before": history_before,
            "history_after": case.event_log("destination", mapping["successor"]),
            "source_document": source_export,
            "target_document": target_export,
        },
    )
    return {
        "session": session,
        "outcome": "carried",
        "mappings": authority["mappings"],
        "operator_output": result.stdout + result.stderr,
        "old_source": "refused",
        "terminal_rows_before": len(before["terminals"]),
        "terminal_rows_after": len(after["terminals"]),
        "job_state_in_document": sorted(
            key for key in source_export if "job" in key or "process" in key
        ),
        "retained_job_warning": "S3a carries conversation into a new line, not a job ledger; read/save terminal output using the old build before bootstrap",
        "terminal_notice_in_history": "retained-rehearsal-output"
        in json.dumps(source_export["history"]),
    }


def reproduce_terminal_job(case):
    session = case.create("job")
    before = case.snapshot("source")
    line = before["branches"][0]["line_id"]
    native_head = case.rows(
        "source", "SELECT session_id,branch_id,checkpoint_id FROM branches", "runtime"
    )[0]
    result = case.upgrade()
    case.s.write_json(
        case.root / "repro-terminal-job.json",
        {
            "session": session,
            "process_finished": (case.path / "job-done").read_text(),
            "root_turns": case.rows("source", "SELECT root_turn_count FROM session_views"),
            "heads": case.rows(
                "source",
                "SELECT b.session_id,b.branch_id,c.kind FROM branches b JOIN checkpoints c ON c.checkpoint_id=b.checkpoint_id",
                "runtime",
            ),
            "exit": result.returncode,
            "refusal": result.stderr,
            "receipts": case.receipts(),
        },
    )
    case.s.require(result.returncode == 0, f"terminal-job move refused: {result.stderr}")
    authority = case.receipts()[-1]
    case.s.require(
        authority["phase"] == "acknowledged"
        and all(row["source"] == row["successor"] for row in authority["mappings"]),
        "terminal job did not preserve its product session",
    )
    case.attach_verify(session)
    after = case.snapshot("destination")
    for field in ["products", "branches", "nodes"]:
        case.s.require(before[field] == after[field], f"terminal-job {field} changed")
    native = {
        row["operation"].lower(): row["digest"].lower()
        for row in case.rows(
            "destination",
            "SELECT hex(operation) AS operation,hex(digest) AS digest FROM runtime_moves",
            "runtime",
        )
    }
    heads = []
    for group in authority["groups"]:
        case.s.require(
            native.get(bytes(group["operation"]).hex()) == group["target_digest"],
            "native target does not attest the source job reconstruction",
        )
        capsule = json.loads(
            (
                Path(case.env["PLAK_HOME"])
                / "host/upgrades"
                / authority["id"]
                / f"{group['id']}.json"
            ).read_text()
        )
        heads.extend(
            head for item in capsule["sessions"] for head in item["heads"] if head["line"] == line
        )
    case.s.require(len(heads) == 1, "retained job line was dropped or duplicated")
    case.s.write_json(
        case.root / "retained-job-source-head.json",
        {"native": native_head, "source": heads[0]["source"]},
    )
    captured = heads[0]["source"].split("/")
    case.s.require(
        len(captured) == 3
        and captured[:2] == [native_head["session_id"], native_head["branch_id"]]
        and case.rows(
            "source",
            "SELECT checkpoint_id FROM checkpoints WHERE checkpoint_id=? AND session_id=? AND branch_id=?",
            "runtime",
            (captured[2], *captured[:2]),
        ),
        "retained job capture did not reference a committed head on the original native branch",
    )
    jobs = heads[0]["jobs"]
    case.s.require(
        len(jobs["rows"]) == 1 and jobs["next_number"] == 2 and jobs["rows"][0]["number"] == 1,
        "retained job identity/number floor changed",
    )
    case.s.require(
        jobs["rows"][0]["terminal"]["tail"] == "retained-rehearsal-output",
        "complete retained fixture output changed",
    )
    unconsumed = terminal_inventory(Path(case.env["PLAK_HOME"]), line, case.s)
    refused = case.command(
        "source", ["run", "--session", session, "--", "must not execute"], check=False
    )
    case.s.require(
        refused.returncode != 0
        and any(code in refused.stderr for code in ["source_fenced", "session_archived"]),
        f"old terminal-job owner was not fenced: {refused.stderr}",
    )
    case.s.require(
        terminal_inventory(Path(case.env["PLAK_HOME"]), line, case.s) == unconsumed,
        "read-only verification or the refused source consumed the moved job",
    )
    case.s.write_json(
        case.root / "retained-job-invariants.json",
        {
            "before": before,
            "after": after,
            "jobs": jobs,
            "unconsumed_after_readonly_checks": unconsumed,
            "groups": authority["groups"],
        },
    )
    return {
        "session": session,
        "outcome": "terminal job identity, full fixture output and number floor moved",
        "jobs": jobs,
        "unconsumed": unconsumed,
        "old_source_refusal": refused.stderr,
    }


def reproduce_finished_child(case):
    session = case.create("child")
    before = case.snapshot("source")
    history = case.exports("source", [session])[session]["history"]
    child = next(row["agent_id"] for row in before["nodes"] if row["parent_agent_id"])
    child_history = case.child_history("source", session, child)
    case.start()
    result = case.upgrade()
    case.s.write_json(
        case.root / "repro-finished-child.json",
        {
            "session": session,
            "root_turns": case.rows("source", "SELECT root_turn_count FROM session_views"),
            "nodes": case.rows(
                "source",
                "SELECT agent_id,parent_agent_id,status,active_turn,session_locator FROM agent_nodes",
                "subagents",
            ),
            "heads": case.rows(
                "source",
                "SELECT b.session_id,b.branch_id,c.kind FROM branches b JOIN checkpoints c ON c.checkpoint_id=b.checkpoint_id",
                "runtime",
            ),
            "exit": result.returncode,
            "refusal": result.stderr,
            "receipts": case.receipts(),
        },
    )
    case.s.require(result.returncode == 0, f"finished-child move refused: {result.stderr}")
    case.s.require(case.snapshot("destination") == before, "finished child IDs or bindings changed")
    case.s.require(
        case.exports("destination", [session])[session]["history"] == history,
        "parent history changed",
    )
    case.s.require(
        case.child_history("destination", session, child) == child_history, "child history changed"
    )
    wake_after_move(case, session, child)
    return {
        "session": session,
        "outcome": "finished child moved; its next result woke the moved idle root once",
    }


def wake_after_move(case, session, child):
    before = case.rows(
        "destination",
        "SELECT root_turn_count FROM session_views WHERE session_id=?",
        parameters=(session,),
    )[0]["root_turn_count"]
    gate = case.path / "after-move-child-release"
    case.s.write_json(
        case.path / "child.json",
        {
            "version": 1,
            "repeat_last_step": True,
            "steps": [
                {
                    "stream": [{"gate": {"file": str(gate)}}],
                    "output": {
                        "tool": {
                            "name": "submit_result",
                            "arguments": {
                                "turn_token": 2,
                                "output_json": '{"summary":"after move"}',
                            },
                        }
                    },
                    "finish": "tool_calls",
                }
            ],
        },
    )
    with case.client("destination") as client:
        node = next(
            row["node"]
            for row in client.request(1, {"op": "list_sessions", "include_archived": False})["rows"]
            if row["session"] == session
        )
        attachment = client.request(2, {"op": "attach", "node": node})["attachment"]
        client.request(
            3,
            {
                "op": "intent",
                "attachment": attachment,
                "blobs": [],
                "intent": {
                    "kind": "invoke_tool",
                    "body": {
                        "correlation_id": None,
                        "name": "send_agent_message",
                        "arguments": json.dumps(
                            {
                                "agent_id": child,
                                "message": "follow up after the move",
                                "purpose": "followup",
                            }
                        ),
                        "agent_id": None,
                    },
                },
            },
        )
        client.request(4, {"op": "detach", "attachment": attachment})
    case.s.wait_for(
        lambda: case.rows(
            "destination",
            "SELECT * FROM agent_nodes WHERE agent_id=? AND active_turn<>0",
            "subagents",
            (child,),
        ),
        "moved child admitted follow-up",
    )
    current = case.rows(
        "destination",
        "SELECT root_turn_count FROM session_views WHERE session_id=?",
        parameters=(session,),
    )[0]["root_turn_count"]
    case.s.require(
        current == before and case.idle(session, before, "destination"),
        "root woke before the child result",
    )
    gate.touch()
    case.s.wait_for(
        lambda: case.idle(session, before + 1, "destination"),
        "moved child result woke the moved root",
    )
    turns = case.rows(
        "destination",
        "SELECT root_turn_count FROM session_views WHERE session_id=?",
        parameters=(session,),
    )[0]["root_turn_count"]
    case.s.require(turns == before + 1, "moved child result caused duplicate root turns")


def rehearse(args, root, support):
    images = json.loads((root / "builds.json").read_text())
    images["checkout"] = root / "owned/checkout-B"
    for name in ["A", "B"]:
        binary = Path(images[name]["binary"])
        support.require(
            binary.is_relative_to(root / "owned/images") and not binary.is_symlink(),
            "image escaped the scratch snapshot directory",
        )
        support.require(
            support.hashlib.sha256(binary.read_bytes()).hexdigest() == images[name]["sha256"],
            f"{name}: binary changed since build",
        )
    report = {
        "images": {key: value for key, value in images.items() if key != "checkout"},
        "cases": [],
        "increment4": "not qualified: live/parked-tree upgrade cases are a separate future extension",
    }
    definitions = []
    if args.arm in {"all", "bootstrap"}:
        definitions += [
            ("b-carry", False, lambda case: carry(case, jobs=False)),
            ("b-child", False, lambda case: bootstrap_observation(case, "active-child")),
            ("b-job", False, lambda case: bootstrap_observation(case, "terminal-job")),
            ("b-live-job", False, lambda case: bootstrap_observation(case, "active-job")),
        ]
    if args.arm in {"all", "dormant"}:
        definitions += [
            ("repro-job", True, reproduce_terminal_job),
            ("repro-child", True, reproduce_finished_child),
            ("d-mixed", True, carry),
        ]
        definitions.extend(INCREMENT4_CASES)
        definitions.append(("d-carry", True, lambda case: carry(case, jobs=False)))
        definitions += [
            (f"d-cut-{index}", True, lambda case, cut=cut: carry(case, cut=cut, jobs=False))
            for index, cut in enumerate(support.CUTS)
        ]
    failed = False
    for name, dormant, body in definitions:
        if args.case and name not in args.case:
            continue
        print(
            f"CASE {name}: {'DORMANT simulated key' if dormant else 'BOOTSTRAP real revisions'}",
            flush=True,
        )
        case = None
        entry = {"case": name, "arm": "dormant" if dormant else "bootstrap"}
        try:
            case = Case(
                support,
                root,
                name,
                images,
                dormant,
                "interrupted" if name.startswith("d-") and name != "d-mixed" else "completed",
            )
            entry.update(body(case), status="passed")
        except Exception as error:
            entry.update(status="failed", error=f"{type(error).__name__}: {error}")
            failed = True
        finally:
            if case:
                case.persist_logs()
            path = root / "owned/cases" / name
            if path.exists():
                support.cleanup(path, case.children if case else [])
            entry["isolation"] = (
                "no live scratch processes; owned leaders reaped and case state removed"
            )
        report["cases"].append(entry)
        support.write_json(root / "report.json", report)
        (root / "operator-summary.md").write_text(operator_summary(report))
        print(json.dumps(entry), flush=True)
    support.require(report["cases"], "selection ran zero rehearsal cases")
    support.require(not failed, "one or more rehearsal cases failed; see report.json")
    return report
